#!/usr/bin/env python3
"""Record a scripted ttmux demo to an asciicast v2 file.

    python3 record.py [--out out/demo.cast] [--until SECONDS] [--keep]

Runs the ttmux client in a pty (COLS x ROWS) against a private session
($TTMUX_SESSION=demo, config ./ttmux.toml), drives it with keystrokes
and SGR mouse reports on a timeline, and writes the client's output as "o"
events plus the pointer as "m" events ("x,y,down|up", 0-based cells).
--until stops the timeline early (for dry runs); --keep leaves the session
running afterwards so it can be inspected with capture-pane.
"""
import codecs, fcntl, json, math, os, pty, random, select, signal, struct
import shutil, subprocess, sys, termios, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
TTMUX = os.path.join(REPO, "target/release/ttmux")
OUT = os.path.join(HERE, "out")
SESSION = "demo"
COLS, ROWS = 170, 48
VIDEO = "~videos/crabs.mp4"  # zsh named dir set in zshrc

ENV = {
    "HOME": os.environ["HOME"],
    "USER": os.environ.get("USER", ""),
    "LOGNAME": os.environ.get("LOGNAME", ""),
    "TMPDIR": os.environ.get("TMPDIR", "/tmp"),  # picks the socket dir
    # The fake `ci`, this checkout's ttmux, and wherever `claude` lives.
    "PATH": ":".join([os.path.join(HERE, "bin"), os.path.dirname(TTMUX),
                      os.path.dirname(shutil.which("claude") or "/"),
                      "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"]),
    "LANG": "en_US.UTF-8",
    "TERM": "xterm-256color",
    "COLORTERM": "truecolor",
    "SHELL": "/bin/zsh",
    "ZDOTDIR": os.path.join(OUT, "zdot"),
    "DEMO_OUT": OUT,
    "DEMO_REPO": REPO,
    "TTMUX_SESSION": SESSION,
    "TTMUX_CONFIG": os.path.join(HERE, "ttmux.toml"),
}
# Ghostty-ish answers to the client's startup probe (OSC 10, OSC 11, DA1).
PROBE_REPLY = (b"\x1b]10;rgb:c0c0/caca/f5f5\x1b\\"
               b"\x1b]11;rgb:1a1a/1b1b/2626\x1b\\"
               b"\x1b[?62;22;52c")
# Queries a real terminal would answer; logged if seen after startup.
QUERIES = [b"\x1b[c", b"\x1b[>c", b"\x1b[0c", b"\x1b[6n", b"\x1b[5n", b"\x1b[?u",
           b"\x1b[>q", b"\x1b[>0q", b"\x1b]10;?", b"\x1b]11;?", b"\x1b[14t", b"\x1b[18t"]


def ttmux(*args):
    """Run a scripting command against the demo session, without waiting:
    blocking here would stop us draining the pty, and the client could
    then back up into the server that is trying to answer."""
    return subprocess.Popen([TTMUX, *args], env=ENV, cwd=REPO,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


class Rec:
    def __init__(self, out):
        self.f = open(out, "w")
        self.f.write(json.dumps({"version": 2, "width": COLS, "height": ROWS,
                                 "timestamp": int(time.time()),
                                 "env": {"TERM": "xterm-256color", "SHELL": "/bin/zsh"}}) + "\n")
        self.t0 = time.monotonic()
        self.dec = codecs.getincrementaldecoder("utf-8")("replace")
        self.ptr = (COLS * 0.6, ROWS * 0.55)
        self.down = False
        self.stopped = False  # set once the timeline ends, before teardown

    def now(self):
        return time.monotonic() - self.t0

    def ev(self, kind, data):
        self.f.write(json.dumps([round(self.now(), 6), kind, data], ensure_ascii=False) + "\n")

    def output(self, b):
        s = self.dec.decode(b)
        if s and not self.stopped:
            self.ev("o", s)

    def pointer(self, x, y):
        self.ptr = (x, y)
        self.ev("m", f"{x:.2f},{y:.2f},{'down' if self.down else 'up'}")


# ------------------------------------------------------------- timeline
# Generators that `yield` seconds to wait; `fd` is the pty master.
REC = None      # the Rec, set in main; `key` labels go to it as "k" events
START = None    # cast time where the shown video begins (after pre-roll)


def keys(fd, b):
    os.write(fd, b if isinstance(b, bytes) else b.encode())


def key(fd, b, label):
    """A shortcut or special key: send it and show it ("k" event)."""
    REC.ev("k", label)
    keys(fd, b)


def typ(fd, text, cps=(0.022, 0.05)):
    for ch in text:
        keys(fd, ch)
        yield random.uniform(*cps) * (2.5 if ch == " " and random.random() < 0.2 else 1)


def csi_u(ch, ctrl=False, alt=False, shift=False):
    """A chord as a kitty-keyboard terminal sends it (ttmux pushes flag 1)."""
    m = 1 + shift * 1 + alt * 2 + ctrl * 4
    return f"\x1b[{ord(ch)};{m}u".encode()


CTRL_ALT_RIGHT = b"\x1b[1;7C"
CTRL_ALT_DOWN = b"\x1b[1;7B"
PREFIX = csi_u("t", ctrl=True)
ENTER = b"\r"
ESC = b"\x1b[27u"


def enter(fd):
    key(fd, ENTER, "↵")


def prefix(fd, k, label=None, gap=0.35):
    key(fd, PREFIX, "⌃T")
    yield gap
    key(fd, k, label or k.upper())


def sgr(fd, code, x, y, release=False):
    keys(fd, f"\x1b[<{code};{int(round(x)) + 1};{int(round(y)) + 1}{'m' if release else 'M'}")


def move(rec, fd, x1, y1, dur=0.6, hz=60):
    """Ease the pointer to (x1, y1), reporting motion (or drag) to ttmux at
    each new cell and the fractional position to the cast every frame."""
    x0, y0 = rec.ptr
    steps = max(1, int(dur * hz))
    last = (int(round(x0)), int(round(y0)))
    for i in range(1, steps + 1):
        u = i / steps
        e = u * u * (3 - 2 * u)  # smoothstep
        x, y = x0 + (x1 - x0) * e, y0 + (y1 - y0) * e
        rec.pointer(x, y)
        cell = (int(round(x)), int(round(y)))
        if cell != last:
            sgr(fd, 32 if rec.down else 35, x, y)
            last = cell
        yield 1 / hz


def press(rec, fd):
    rec.down = True
    sgr(fd, 0, *rec.ptr)
    rec.pointer(*rec.ptr)


def release(rec, fd):
    rec.down = False
    sgr(fd, 0, *rec.ptr, release=True)
    rec.pointer(*rec.ptr)


def click(rec, fd, x, y, dur=0.5):
    yield from move(rec, fd, x, y, dur)
    yield 0.12
    press(rec, fd)
    yield 0.09
    release(rec, fd)


def drag(rec, fd, x0, y0, x1, y1, dur=0.8, hold=0.25):
    yield from move(rec, fd, x0, y0, 0.5)
    yield 0.15
    press(rec, fd)
    yield 0.15
    yield from move(rec, fd, x1, y1, dur)
    yield hold
    release(rec, fd)


# Geometry (170x48; panes fill rows 0..46, the footer is row 47), checked
# against screen dumps of dry runs.
L, R = 0, 85             # left / right column x after the first split
MID = 24                 # row where a 47-row column splits
MPV_W = 42               # mpv's width after the bottom-right split


def preroll(rec, fd):
    """Setup the shown video starts after: nvim with the file tree."""
    yield 0.8
    yield from typ(fd, "nvim src/render.rs")
    yield 0.35
    keys(fd, ENTER)
    yield 1.4
    yield from typ(fd, "\\F", (0.12, 0.15))       # nvim-tree, revealing render.rs
    yield 1.4
    keys(fd, csi_u("w", ctrl=True)); yield 0.1; keys(fd, "l")
    yield 0.4
    for _ in range(4):
        keys(fd, "}"); yield 0.1
    yield 1.0


def timeline(rec, fd):
    global START
    yield from preroll(rec, fd)
    START = rec.now() - 0.4

    # 1. Split right: a busy CI run in the big pane (agent spinner).
    key(fd, CTRL_ALT_RIGHT, "⌃⌥→")
    yield 0.9
    yield from typ(fd, "ci")
    yield 0.25
    enter(fd)
    yield 1.6

    # 2. Split it down: Crab Rave, small, as truecolor character video.
    key(fd, CTRL_ALT_DOWN, "⌃⌥↓")
    yield 0.8
    yield from typ(fd, f"mpv --vo=tct --really-quiet --no-audio --loop {VIDEO}")
    yield 0.3
    enter(fd)
    yield 1.8

    # 3. And right again: cmatrix.
    key(fd, CTRL_ALT_RIGHT, "⌃⌥→")
    yield 0.7
    yield from typ(fd, "cmatrix -b")
    yield 0.2
    enter(fd)
    yield 1.2

    # 4. Rearrange by dragging panes by their titles onto another pane's
    #    side; the accent preview shows where it lands.
    cx = R + MPV_W
    yield from drag(rec, fd, cx + 5, MID, 40, 41, dur=1.1, hold=1.1)     # cmatrix -> under nvim
    yield 0.7
    yield from drag(rec, fd, R + 5, MID, 128, 3, dur=1.0, hold=1.1)     # crabs -> above CI
    yield 0.7
    yield from drag(rec, fd, R + 5, 0, 79, 36, dur=1.1, hold=1.1)       # crabs -> right of cmatrix
    yield 1.2

    # 5. Resize by dragging the column divider.
    yield from drag(rec, fd, R - 1, 12, 76, 12, dur=0.7, hold=0.2)
    yield 0.8

    # 6. Zoom the CI log for a moment.
    yield from click(rec, fd, 125, 20)
    yield 0.2
    yield from prefix(fd, "z")
    yield 1.6
    yield from prefix(fd, "z")
    yield 0.8

    # 7. Free mode: the grid becomes floating windows.
    key(fd, csi_u("f", ctrl=True, alt=True), "⌃⌥F")
    yield 1.2
    if FREE_MPV:
        yield from drag(rec, fd, *FREE_MPV, dur=1.0, hold=0.2)
        yield 0.5
    if FREE_CMX:
        yield from drag(rec, fd, *FREE_CMX, dur=0.8, hold=0.2)
    yield 1.0
    key(fd, csi_u("f", ctrl=True, alt=True), "⌃⌥F")
    yield 1.1

    # 8. The prefix shows what can follow it; ? opens the full help.
    key(fd, PREFIX, "⌃T")
    yield 2.4
    key(fd, b"?", "?")
    yield 2.0
    for _ in range(3):
        key(fd, b"j", "J"); yield 0.4
    yield 1.2
    key(fd, ESC, "⎋")
    yield 0.8

    # 9. Settings: scroll through, change nothing, close.
    yield from prefix(fd, ",")
    yield 1.4
    for _ in range(5):
        key(fd, b"j", "J"); yield 0.32
    yield 0.6
    key(fd, b"\t", "⇥")
    yield 0.5
    for _ in range(3):
        key(fd, b"j", "J"); yield 0.75
    yield 0.5
    key(fd, ESC, "⎋")
    yield 0.8

    # 10. Name this window "demo".
    yield from prefix(fd, "A", "⇧A")
    yield 0.7
    key(fd, csi_u("u", ctrl=True), "⌃U")
    yield 0.3
    yield from typ(fd, "demo", (0.08, 0.12))
    yield 0.3
    enter(fd)
    yield 0.9

    # 11. A second window, "claude", with Claude Code at work in it.
    key(fd, csi_u("t", ctrl=True, alt=True), "⌃⌥T")
    yield 0.6
    yield from prefix(fd, "A", "⇧A")
    yield 0.5
    key(fd, csi_u("u", ctrl=True), "⌃U")
    yield 0.2
    yield from typ(fd, "claude", (0.07, 0.1))
    yield 0.2
    enter(fd)
    yield 0.6
    yield from typ(fd, "claude")
    yield 0.2
    enter(fd)
    yield 3.0
    yield from typ(fd, CLAUDE_PROMPT, (0.015, 0.03))
    yield 0.4
    enter(fd)
    yield 2.0

    # 12. Back to "demo": the claude tab keeps spinning, then ticks.
    yield from prefix(fd, "p")
    yield 0.6
    yield from move(rec, fd, 120, 40, 0.8)
    yield HOLD_END


CLAUDE_PROMPT = "count the #[test] fns per file in src/, top 3?"
HOLD_END = 7.5
# Filled in from dry-run screen dumps: (x0, y0, x1, y1) drags in free mode.
FREE_MPV = (45, 24, 100, 6)
FREE_CMX = (38, 46, 70, 44)


def main():
    global UNTIL
    args = sys.argv[1:]
    out = os.path.join(OUT, "demo.cast")
    UNTIL, keep = None, False
    while args:
        a = args.pop(0)
        if a == "--out": out = args.pop(0)
        elif a == "--until": UNTIL = float(args.pop(0))
        elif a == "--keep": keep = True
        else: sys.exit(__doc__)
    random.seed(1)
    os.makedirs(os.path.join(OUT, "zdot"), exist_ok=True)
    shutil.copy(os.path.join(HERE, "zshrc"), os.path.join(OUT, "zdot", ".zshrc"))

    # A stale demo session from an aborted run would be reattached.
    subprocess.run([TTMUX, "kill-session", "-t", SESSION], env=ENV,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    pid, fd = pty.fork()
    if pid == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        os.chdir(REPO)
        os.execve(TTMUX, [TTMUX], ENV)  # bare ttmux creates $TTMUX_SESSION
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    rec = Rec(out)
    global REC
    REC = rec
    tl = timeline(rec, fd)
    due = rec.now()
    probed, tail, seen = False, b"", {}
    done = False
    while True:
        timeout = max(0.0, due - rec.now()) if not done else 0.3
        r, _, _ = select.select([fd], [], [], timeout)
        if r:
            try:
                b = os.read(fd, 65536)
            except OSError:
                break
            if not b:
                break
            rec.output(b)
            scan = tail + b
            if not probed and b"\x1b[c" in scan:
                os.write(fd, PROBE_REPLY)
                probed, scan = True, b""
            elif probed:
                for q in QUERIES:
                    if q in scan:
                        seen[q] = seen.get(q, 0) + 1
            tail = scan[-16:]
        if done:
            continue
        while rec.now() >= due:
            try:
                if UNTIL and rec.now() > UNTIL:
                    raise StopIteration
                due = rec.now() + next(tl)
            except StopIteration:
                done = True
                rec.pointer(*rec.ptr)  # hold the last screen to here
                rec.stopped = True
                if keep:
                    print(f"kept session {SESSION}; t={rec.now():.1f}s", file=sys.stderr)
                    os.kill(pid, signal.SIGTERM)  # detach-ish: leave the server up
                else:
                    ttmux("kill-session", "-t", SESSION).wait()
                break
    os.waitpid(pid, 0)
    rec.f.close()
    if START is not None:
        with open(os.path.join(OUT, "start.txt"), "w") as f:
            f.write(f"{START:.2f}\n")
    print(f"start={START}", file=sys.stderr)
    print(f"wrote {out}: {rec.now():.1f}s, {COLS}x{ROWS}; probe answered={probed}",
          file=sys.stderr)
    if seen:
        print("unanswered queries seen later:", {k.decode('latin1').replace('\x1b', 'ESC'): v
                                                 for k, v in seen.items()}, file=sys.stderr)


if __name__ == "__main__":
    main()
