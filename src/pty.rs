//! One pseudo-terminal per pane: child process, terminal emulator, scrollback.
//!
//! Output is read on a background thread into a channel; [`Pane::pump`] drains
//! that channel into the emulator and never blocks.

use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

use anyhow::Context;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

use crate::config::Config;
use crate::graphics::{Image, Piece, Scanner};
use crate::layout::PaneId;

const READ_BUF: usize = 64 * 1024;
/// Most bytes one `pump()` will feed the emulator, so a noisy pane can't
/// starve the UI.
const PUMP_BUDGET: usize = 4 * 1024 * 1024;
/// Images kept for the app to collect; past this the oldest is dropped. The
/// app takes them every frame, so this only bounds a pane that draws while
/// nobody is looking.
// ponytail: counted, not sized, so the worst case is this many times
// `graphics::MAX_SEQ`. Bound it by bytes if panes ever hold large images.
const MAX_PENDING_IMAGES: usize = 32;

/// Emulator callbacks: everything vt100 reports outside the screen grid.
#[derive(Default)]
struct Sink {
    title: String,
    bell: bool,
}

impl vt100::Callbacks for Sink {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bell = true;
    }
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        // A pane can put anything here; a TAB or other control character draws
        // at a width nobody agrees on and corrupts the status bar's tab row.
        self.title = String::from_utf8_lossy(title)
            .chars()
            .filter(|c| !c.is_control())
            .collect();
    }
}

/// Rewrites HVP (`CSI … f`) into its identical twin CUP (`CSI … H`).
///
/// ECMA-48 defines the two to do the same thing, but vt100 0.16 implements
/// only `H` and drops `f` on the floor. A program that repositions with `f`
/// — mpv's `--vo=tct` does it once per row of every frame — then has every
/// move ignored, so its frame paints as one long wrapping stream that scrolls
/// the pane and leaves two half-frames on screen at once.
///
/// Stateful because a sequence can straddle two reads from the pty.
// ponytail: swapping the byte beats forking vt100; drop this if vt100 ever
// grows HVP.
#[derive(Default)]
struct Hvp {
    /// Saw `ESC`, waiting to see whether `[` follows.
    esc: bool,
    /// Inside `CSI …` with only parameter bytes so far.
    csi: bool,
}

impl Hvp {
    fn fix(&mut self, buf: &mut [u8]) {
        for b in buf {
            match *b {
                // An ESC anywhere, mid-sequence included, starts over.
                0x1b => {
                    self.esc = true;
                    self.csi = false;
                }
                b'[' if self.esc => {
                    self.esc = false;
                    self.csi = true;
                }
                _ if self.esc => self.esc = false,
                // Parameter bytes keep the sequence open.
                0x30..=0x3b if self.csi => {}
                // Intermediates and the private markers `<=>?` mean this is
                // something other than a plain HVP; stop watching it.
                0x20..=0x3f if self.csi => self.csi = false,
                // Final byte.
                0x40..=0x7e if self.csi => {
                    if *b == b'f' {
                        *b = b'H';
                    }
                    self.csi = false;
                }
                _ => {}
            }
        }
    }
}

/// One pane: the child, its pty, and the emulator the app draws from.
pub struct Pane {
    pub id: PaneId,
    pub cols: u16,
    pub rows: u16,
    /// Rows scrolled back; 0 = live.
    pub scroll: usize,
    /// Sticky bell flag, cleared by the app.
    pub bell: bool,
    pub title_override: Option<String>,

    parser: vt100::Parser<Sink>,
    scanner: Scanner,
    hvp: Hvp,
    /// Graphics sequences captured since the app last took them.
    images: RefCell<Vec<Image>>,
    rx: Receiver<Vec<u8>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: RefCell<Box<dyn Child + Send + Sync>>,
    exit: Cell<Option<u32>>,
    /// `kill` already waited: signalling the group again could hit a pid the
    /// kernel has since handed to someone else.
    reaped: Cell<bool>,
    /// Write side failed; stop writing. Reads still drain.
    disconnected: bool,
    /// Fallback title: the program's basename.
    name: String,
}

impl Pane {
    /// Spawn the configured shell in a new pty.
    pub fn spawn(
        id: PaneId,
        cfg: &Config,
        cwd: Option<PathBuf>,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<Pane> {
        let shell = if !cfg.general.shell.is_empty() {
            cfg.general.shell.clone()
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
        };
        let mut cmd = CommandBuilder::new(&shell);
        cmd.args(&cfg.general.shell_args);
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        cmd.env("TERM", "xterm-256color");
        // Every colour a pane emits reaches the outer terminal as 24-bit, so
        // say so: without it a program downgrades to the 16 ANSI colours and
        // paints, say, a badge as reverse video.
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TTMUX", "1");
        cmd.env("TTMUX_PANE", id.to_string());
        Pane::spawn_cmd(id, cmd, cfg.general.scrollback, cols, rows)
    }

    /// Test/headless constructor: runs `cmd` instead of the shell.
    pub fn spawn_cmd(
        id: PaneId,
        cmd: CommandBuilder,
        scrollback: usize,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<Pane> {
        // vt100's grid does `rows - 1`, so a zero size underflows u16.
        let (cols, rows) = (cols.max(1), rows.max(1));
        let name = cmd.get_argv().first().map_or_else(String::new, |p| {
            PathBuf::from(p)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });

        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;
        let child = pair.slave.spawn_command(cmd).context("spawn")?;
        // Drop our copy of the slave so the reader sees EOF when the child exits.
        drop(pair.slave);

        let writer = pair.master.take_writer().context("pty writer")?;
        let mut reader = pair.master.try_clone_reader().context("pty reader")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; READ_BUF];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Pane {
            id,
            cols,
            rows,
            scroll: 0,
            bell: false,
            title_override: None,
            parser: vt100::Parser::new_with_callbacks(rows, cols, scrollback, Sink::default()),
            scanner: Scanner::new(),
            hvp: Hvp::default(),
            images: RefCell::new(Vec::new()),
            rx,
            master: pair.master,
            writer,
            child: RefCell::new(child),
            exit: Cell::new(None),
            reaped: Cell::new(false),
            disconnected: false,
            name,
        })
    }

    /// Resize both the emulator and the child's pty.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if (cols, rows) == (self.cols, self.rows) || cols == 0 || rows == 0 {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.parser.screen_mut().set_size(rows, cols);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    /// Write bytes to the child. After an I/O error, writes stop; the
    /// already-buffered output still drains through [`Pane::pump`].
    pub fn send(&mut self, bytes: &[u8]) {
        if self.disconnected {
            return;
        }
        if self.writer.write_all(bytes).is_err() || self.writer.flush().is_err() {
            self.disconnected = true;
        }
    }

    /// Feed pending child output into the emulator. True if anything changed.
    pub fn pump(&mut self) -> bool {
        let mut got = 0usize;
        while got < PUMP_BUDGET {
            match self.rx.try_recv() {
                // try_recv reports Disconnected only once the queue is empty,
                // so breaking on any error still hands over every byte the
                // reader managed to send before it died.
                Err(_) => break,
                Ok(chunk) => {
                    got += chunk.len();
                    for piece in self.scanner.feed(&chunk) {
                        match piece {
                            Piece::Plain(mut bytes) => {
                                self.hvp.fix(bytes.to_mut());
                                self.parser.process(&bytes);
                            }
                            // The cursor is wherever the preceding plain bytes
                            // left it, which is where the image belongs.
                            Piece::Image(bytes) => {
                                let (row, col) = self.parser.screen().cursor_position();
                                let mut pending = self.images.borrow_mut();
                                if pending.len() >= MAX_PENDING_IMAGES {
                                    pending.remove(0);
                                }
                                pending.push(Image { row, col, bytes });
                            }
                        }
                    }
                }
            }
        }
        if got == 0 {
            return false;
        }
        if std::mem::take(&mut self.parser.callbacks_mut().bell) {
            self.bell = true;
        }
        // New output only reaches the live view; keep the scrollback offset put.
        self.parser.screen_mut().set_scrollback(self.scroll);
        true
    }

    /// Graphics sequences captured since the last call, oldest first; the
    /// pane keeps none of them. Cells are the cursor position at capture
    /// time, so scrolling since then makes them stale. A pane that captured
    /// more than `MAX_PENDING_IMAGES` between calls has silently lost the
    /// oldest, so calling every frame is not optional.
    pub fn take_images(&self) -> Vec<Image> {
        std::mem::take(&mut self.images.borrow_mut())
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Override, else the emulator's OSC title, else the program name.
    pub fn title(&self) -> String {
        if let Some(t) = &self.title_override {
            return t.clone();
        }
        let osc = &self.parser.callbacks().title;
        if !osc.is_empty() {
            return osc.clone();
        }
        self.name.clone()
    }

    /// The child's working directory, for opening new panes in the same place.
    pub fn cwd(&self) -> Option<PathBuf> {
        let pid = self.child.borrow().process_id()?;
        cwd_of(pid)
    }

    pub fn is_dead(&self) -> bool {
        self.exit_status().is_some()
    }

    pub fn exit_status(&self) -> Option<u32> {
        if let Some(code) = self.exit.get() {
            return Some(code);
        }
        if let Ok(Some(st)) = self.child.borrow_mut().try_wait() {
            self.exit.set(Some(st.exit_code()));
        }
        self.exit.get()
    }

    /// Negative scrolls back into history, positive returns toward live.
    pub fn scroll_by(&mut self, delta: isize) {
        self.scroll = (self.scroll as isize - delta).max(0) as usize;
        self.parser.screen_mut().set_scrollback(self.scroll);
        // vt100 clamps to the real history length; mirror what it settled on.
        self.scroll = self.parser.screen().scrollback();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
        self.parser.screen_mut().set_scrollback(0);
    }

    /// Recent plain text from the screen, for agent detection.
    pub fn tail(&self, lines: usize) -> String {
        let screen = self.parser.screen();
        let rows: Vec<String> = screen
            .rows(0, self.cols)
            .filter(|r| !r.trim().is_empty())
            .collect();
        let start = rows.len().saturating_sub(lines);
        rows[start..].join("\n")
    }

    /// Kill the child and everything it left behind, then reap it.
    pub fn kill(&mut self) {
        if self.reaped.get() {
            return;
        }
        self.reaped.set(true);
        let mut child = self.child.borrow_mut();
        // portable_pty setsid()s before exec, so the child's pid is also its
        // process group id.
        #[cfg(unix)]
        let pg = child.process_id().map(|p| p as libc::pid_t);
        #[cfg(unix)]
        if let Some(pg) = pg {
            unsafe { libc::killpg(pg, libc::SIGHUP) };
        }
        let _ = child.kill();
        #[cfg(unix)]
        if let Some(pg) = pg {
            // A grandchild that outlives the shell keeps the pty slave open, so
            // the reader thread would sit in read() forever holding the fd.
            unsafe { libc::killpg(pg, libc::SIGKILL) };
            // A shell with job control puts `cmd &` in a process group of its
            // own, which the killpg above never reaches. The session is the
            // one grouping that holds every descendant, and setsid() made the
            // child's pid its session id.
            //
            // ponytail: shells out to pkill; a /proc walk plus a sysctl for
            // macOS is the alternative, and it is a lot of code for a path
            // that runs once per pane at exit.
            let _ = std::process::Command::new("pkill")
                .args(["-9", "-s", &pg.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        // Nothing else waits on this child once the app drops the pane, and
        // Child's Drop doesn't reap: without this it stays <defunct>.
        if let Ok(st) = child.wait() {
            self.exit.set(Some(st.exit_code()));
        }
    }
}

impl Drop for Pane {
    /// Quitting drops panes without calling `kill`, and closing the master is
    /// not enough: a backgrounded grandchild ignores the hangup and outlives
    /// the app. Reap the group here so every exit path is covered.
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(target_os = "linux")]
fn cwd_of(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(target_os = "macos")]
fn cwd_of(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut info as *mut libc::proc_vnodepathinfo).cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    // vip_path is a flattened [c_char; MAXPATHLEN], NUL-terminated.
    let raw = &info.pvi_cdir.vip_path;
    let bytes: Vec<u8> = raw
        .iter()
        .flatten()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    if bytes.is_empty() {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes)))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn cwd_of(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn sh(script: &str) -> CommandBuilder {
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args(["-c", script]);
        cmd.env("TERM", "xterm-256color");
        cmd
    }

    fn pane(script: &str, cols: u16, rows: u16) -> Pane {
        Pane::spawn_cmd(1, sh(script), 1000, cols, rows).unwrap()
    }

    /// Pump until `f` holds or 5s pass. Returns whether it held.
    fn pump_until(p: &mut Pane, f: impl Fn(&Pane) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            p.pump();
            if f(p) {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn output_reaches_the_screen() {
        let mut p = pane("printf hello", 40, 10);
        assert!(pump_until(&mut p, |p| p
            .screen()
            .contents()
            .contains("hello")));
    }

    #[test]
    fn exit_status_is_reported() {
        let mut p = pane("exit 3", 40, 10);
        assert!(pump_until(&mut p, |p| p.exit_status().is_some()));
        assert!(p.is_dead());
        assert_eq!(p.exit_status(), Some(3));
    }

    #[test]
    fn send_reaches_the_child() {
        let mut p = pane("read x; printf 'got:%s' \"$x\"", 40, 10);
        // Wait for the child to be up before typing at it.
        std::thread::sleep(Duration::from_millis(100));
        p.send(b"abc\n");
        assert!(pump_until(&mut p, |p| p
            .screen()
            .contents()
            .contains("got:abc")));
    }

    #[test]
    fn resize_changes_the_emulator_size() {
        let mut p = pane("printf hi; sleep 1", 40, 10);
        assert!(pump_until(&mut p, |p| p.screen().contents().contains("hi")));
        p.resize(60, 20);
        assert_eq!(p.screen().size(), (20, 60));
        assert_eq!((p.cols, p.rows), (60, 20));
    }

    #[test]
    fn scrollback_moves_the_view() {
        let mut p = pane(
            "i=1; while [ $i -le 200 ]; do echo line$i; i=$((i+1)); done",
            40,
            10,
        );
        assert!(pump_until(&mut p, |p| p
            .screen()
            .contents()
            .contains("line200")));
        let live = p.screen().contents();
        p.scroll_by(-5);
        assert_eq!(p.scroll, 5);
        assert_ne!(p.screen().contents(), live);
        p.scroll_to_bottom();
        assert_eq!(p.screen().contents(), live);
    }

    #[test]
    fn tail_returns_the_last_lines() {
        let mut p = pane("printf 'a\\nb\\nc\\nd\\n'", 40, 10);
        assert!(pump_until(&mut p, |p| p.screen().contents().contains('d')));
        let t = p.tail(3);
        assert_eq!(
            t.lines().map(str::trim).collect::<Vec<_>>(),
            ["b", "c", "d"]
        );
    }

    #[test]
    fn osc_sets_the_title() {
        let mut p = pane("printf '\\033]0;mytitle\\007'", 40, 10);
        assert!(pump_until(&mut p, |p| p.title() == "mytitle"));
    }

    #[test]
    fn a_pane_advertises_truecolor() {
        let mut cfg = Config::default();
        cfg.general.shell = "/bin/sh".into();
        cfg.general.shell_args = vec!["-c".into(), "printf %s \"$COLORTERM\"".into()];
        let mut p = Pane::spawn(1, &cfg, None, 40, 4).unwrap();
        assert!(pump_until(&mut p, |p| p
            .screen()
            .contents()
            .contains("truecolor")));
    }

    #[test]
    fn a_manual_name_outlives_the_title_the_program_sets() {
        let mut p = pane("printf '\\033]0;vim\\007'", 40, 10);
        assert!(pump_until(&mut p, |p| p.title() == "vim"));
        p.title_override = Some("logs".into());
        assert_eq!(p.title(), "logs");
    }

    #[test]
    fn osc_title_drops_control_characters() {
        // C0 never survives the OSC parser; a C1 (here U+0085) does.
        let mut p = pane("printf '\\033]0;a\\302\\205b\\007'", 40, 10);
        assert!(pump_until(&mut p, |p| p.title() == "ab"));
    }

    #[test]
    fn a_graphics_sequence_is_captured_at_the_cursor_it_started_on() {
        // "ab" on row 1, then a kitty APC, then "cd" where the APC was: the
        // sequence never reaches vt100, and its cell is where "cd" begins.
        let mut p = pane("printf 'x\\nab\\033_Ga=T;PAYLOAD\\033\\\\cd'", 40, 10);
        assert!(pump_until(&mut p, |p| p
            .screen()
            .contents()
            .contains("abcd")));
        let imgs = p.take_images();
        assert_eq!(imgs.len(), 1);
        assert_eq!((imgs[0].row, imgs[0].col), (1, 2));
        assert_eq!(imgs[0].bytes, b"\x1b_Ga=T;PAYLOAD\x1b\\");
        // Taking clears them.
        assert!(p.take_images().is_empty());
        // And no part of the payload landed on the screen.
        assert!(!p.screen().contents().contains("PAYLOAD"));
    }

    #[test]
    fn hvp_moves_the_cursor_the_way_cup_does() {
        // vt100 implements only CUP; without the rewrite the escape is dropped
        // and the text lands wherever the cursor happened to be.
        let mut p = pane("printf '\\033[3;5fX\\033[0;1fY'", 40, 10);
        assert!(pump_until(&mut p, |p| p.screen().contents().contains('X')));
        assert_eq!(p.screen().cell(2, 4).map(|c| c.contents()), Some("X"));
        // A row or column of 0 means 1, same as for CUP.
        assert_eq!(p.screen().cell(0, 0).map(|c| c.contents()), Some("Y"));
    }

    #[test]
    fn hvp_rewriting_survives_a_split_read_and_leaves_everything_else_alone() {
        let mut h = Hvp::default();
        // Split mid-sequence: the pty hands over whatever arrived.
        let (mut a, mut b) = (b"\x1b[12".to_vec(), b";7fhi".to_vec());
        h.fix(&mut a);
        h.fix(&mut b);
        assert_eq!([a, b].concat(), b"\x1b[12;7Hhi");

        // Private and intermediate forms happen to end in `f`, and mean
        // something else entirely; an `f` outside a CSI is just a letter.
        let mut other = b"\x1b[?7f f \x1b[ f\x1bf".to_vec();
        let before = other.clone();
        h.fix(&mut other);
        assert_eq!(other, before);
    }

    #[test]
    fn zero_size_does_not_underflow() {
        let p = Pane::spawn_cmd(1, sh("exit 0"), 100, 0, 0).unwrap();
        assert_eq!(p.screen().size(), (1, 1));
    }

    #[test]
    fn a_dead_write_side_still_drains_output() {
        let mut p = pane("printf hello", 40, 10);
        // What send() does on a write error; it must not gate the drain.
        p.disconnected = true;
        assert!(pump_until(&mut p, |p| p
            .screen()
            .contents()
            .contains("hello")));
    }

    #[test]
    fn pending_images_stop_at_the_cap_by_dropping_the_oldest() {
        let mut p = pane(
            "i=0; while [ $i -lt 40 ]; do printf '\\033_Gn=%s;\\033\\\\' $i; \
             i=$((i+1)); done; printf done",
            40,
            10,
        );
        assert!(pump_until(&mut p, |p| p
            .screen()
            .contents()
            .contains("done")));
        let imgs = p.take_images();
        assert_eq!(imgs.len(), MAX_PENDING_IMAGES);
        assert_eq!(imgs[0].bytes, b"\x1b_Gn=8;\x1b\\");
        assert_eq!(imgs[MAX_PENDING_IMAGES - 1].bytes, b"\x1b_Gn=39;\x1b\\");
    }

    /// Both assertions share one pty on purpose: as two tests they each open
    /// a pty and roughly 30% of macOS runs failed with "failed to openpty".
    /// Do not split them apart again.
    #[test]
    fn kill_reaps_the_child_and_its_group() {
        // The shell ignores SIGHUP (so the kill escalates to SIGKILL, the path
        // that never waits) and leaves a grandchild holding the pty slave.
        let mut p = pane("trap '' HUP; (sleep 30) & printf 'up'; wait", 40, 10);
        assert!(pump_until(&mut p, |p| p.screen().contents().contains("up")));
        // setsid() in the spawn makes the child's pid its process group id.
        let pid = p.child.borrow().process_id().unwrap() as libc::pid_t;
        p.kill();

        let mut st = 0;
        // ECHILD (-1) means already reaped; 0 would mean a live child or a
        // <defunct> one still waiting for someone to collect it.
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) },
            -1,
            "child was not reaped"
        );
        // A grandchild that outlives the shell keeps the pty slave open, so the
        // reader thread never sees EOF and its fd leaks for the whole session.
        let deadline = Instant::now() + Duration::from_secs(5);
        while unsafe { libc::killpg(pid, 0) } == 0 {
            assert!(Instant::now() < deadline, "process group survived kill()");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
