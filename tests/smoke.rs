//! End-to-end: run the real `ttmux` binary inside a pty, type at it, and read
//! back what it painted. This is the test that catches "it builds but the
//! screen is blank".

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

struct Harness {
    _child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    parser: vt100::Parser,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    sock: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn start(cols: u16, rows: u16) -> Harness {
        Harness::spawn(cols, rows, true)
    }

    /// No config on disk, which is what puts the app on the welcome screen.
    fn start_first_run(cols: u16, rows: u16) -> Harness {
        Harness::spawn(cols, rows, false)
    }

    fn spawn(cols: u16, rows: u16, configured: bool) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();

        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_ttmux"));
        // A predictable shell and a throwaway config, so the test does not
        // depend on the developer's dotfiles.
        cmd.env("TERM", "xterm-256color");
        cmd.env("SHELL", "/bin/sh");
        cmd.env("PS1", "$ ");
        let cfg_path = dir.path().join("ttmux.toml");
        // An empty file is a default config, and its mere existence is what
        // tells the app this is not a first run. Without it every smoke test
        // would open on the keymap picker.
        if configured {
            std::fs::write(&cfg_path, "").unwrap();
        }
        cmd.env("TTMUX_CONFIG", &cfg_path);
        cmd.env("TTMUX_SESSION", "test");
        // Its own server, in its own directory: without this every test in
        // this file would attach to the same session and to the developer's.
        let sock = dir.path().join("ttmux.sock");
        cmd.env("TTMUX_SOCKET", &sock);
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });

        Harness {
            writer: pair.master.take_writer().unwrap(),
            _child: child,
            parser: vt100::Parser::new(rows, cols, 0),
            rx,
            sock,
            _dir: dir,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn screen(&self) -> String {
        self.parser.screen().contents()
    }

    /// Pump output until `f` matches the screen, or fail after 10s.
    fn wait_for(&mut self, what: &str, f: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            while let Ok(chunk) = self.rx.try_recv() {
                self.parser.process(&chunk);
            }
            if f(&self.screen()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "timed out waiting for {what}; screen was:\n{}",
            self.screen()
        );
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self._child.kill();
        // Killing the client leaves the session server running -- that is the
        // point of it -- so the test has to end the session as well.
        if let Ok(mut sock) = std::os::unix::net::UnixStream::connect(&self.sock) {
            let _ = sock.set_read_timeout(Some(Duration::from_secs(10)));
            let _ = ttmux::proto::write_msg(&mut sock, &ttmux::proto::ClientMsg::KillServer);
            let _ = sock.read_to_end(&mut Vec::new());
        }
    }
}

#[test]
fn it_starts_draws_a_pane_and_runs_a_command() {
    let mut h = Harness::start(80, 24);

    // A bordered pane and the status bar show up on their own.
    h.wait_for("the first pane border", |s| {
        s.contains('╭') && s.contains('╯')
    });
    h.wait_for("the status bar", |s| s.contains("test"));

    // The shell inside the pane is live. The quotes mean the echoed input
    // reads `t''tmux-ok`, so a match proves we are seeing real *output*, not
    // just the characters we typed.
    h.send(b"echo t''tmux-ok\r");
    h.wait_for("command output", |s| s.contains("ttmux-ok"));
}

#[test]
fn prefix_percent_splits_the_window() {
    let mut h = Harness::start(80, 24);
    h.wait_for("the first pane", |s| s.contains('╭'));
    let before = h.screen().matches('╭').count();

    h.send(b"\x14v"); // ctrl+t v, vim vsplit
    h.wait_for("a second pane", |s| s.matches('╭').count() > before);

    // Both panes are live shells, and typing goes to the new one.
    h.send(b"echo new''-pane-ok\r");
    h.wait_for("output in the new pane", |s| s.contains("new-pane-ok"));
}

#[test]
fn the_help_overlay_opens_and_closes() {
    let mut h = Harness::start(80, 24);
    h.wait_for("the first pane", |s| s.contains('╭'));

    h.send(b"\x14?"); // ctrl+t ?
    h.wait_for("the help overlay", |s| {
        s.contains("help") && s.contains("split")
    });

    h.send(b"\x1b"); // any key closes it
    h.wait_for("help to close", |s| !s.contains("toggle-float"));
}

#[test]
fn the_settings_overlay_opens() {
    let mut h = Harness::start(80, 24);
    h.wait_for("the first pane", |s| s.contains('╭'));

    h.send(b"\x14,"); // ctrl+t ,
    h.wait_for("the settings panel", |s| {
        s.contains("Appearance") && s.contains("General")
    });
}

#[test]
fn quitting_exits_cleanly() {
    let mut h = Harness::start(80, 24);
    h.wait_for("the first pane", |s| s.contains('╭'));

    h.send(b"\x14Q"); // ctrl+t shift+q
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(Some(status)) = h._child.try_wait() {
            assert!(status.success(), "ttmux exited with {status:?}");
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("ttmux did not exit after ctrl+t shift+q");
}

/// Print a real session. `cargo test --test smoke -- --ignored --nocapture`.
#[test]
#[ignore = "visual aid, not an assertion"]
fn snapshot() {
    let mut h = Harness::start(100, 28);
    h.wait_for("the first pane", |s| s.contains('╭'));
    h.send(b"\x14%");
    h.wait_for("a second pane", |s| s.matches('╭').count() > 1);
    h.send(b"\x14s"); // ctrl+t s, vim split
    h.wait_for("a third pane", |s| s.matches('╭').count() > 2);
    h.send(b"echo hello from t''tmux\r");
    h.wait_for("output", |s| s.contains("hello from ttmux"));
    println!("{}", h.screen());
}

/// SGR (1006) mouse report: press/drag/release at 1-based (col, row).
fn mouse(button: u8, col: u16, row: u16, release: bool) -> Vec<u8> {
    format!(
        "\x1b[<{};{};{}{}",
        button,
        col + 1,
        row + 1,
        if release { 'm' } else { 'M' }
    )
    .into_bytes()
}

#[test]
fn clicking_a_pane_moves_focus_and_dragging_a_divider_resizes() {
    let mut h = Harness::start(80, 24);
    h.wait_for("the first pane", |s| s.contains('╭'));
    h.send(b"\x14%"); // split right; focus moves to the right-hand pane
    h.wait_for("a second pane", |s| s.matches('╭').count() > 1);

    // The divider between the two panes sits at column 39/40. Grab it and drag
    // it left; the left pane's top border must get shorter.
    let width_of_first_pane = |s: &str| s.lines().next().unwrap_or("").find('╮').unwrap_or(0);
    let before = width_of_first_pane(&h.screen());
    h.send(&mouse(0, 39, 10, false));
    h.send(&mouse(32, 25, 10, false)); // drag (button 0 + 32)
    h.send(&mouse(0, 25, 10, true));
    h.wait_for("the divider to move left", |s| {
        width_of_first_pane(s) < before
    });

    // Clicking inside the left pane focuses it: type and the text lands there.
    h.send(&mouse(0, 5, 5, false));
    h.send(&mouse(0, 5, 5, true));
    h.send(b"echo left''-pane-ok\r");
    h.wait_for("output in the left pane", |s| {
        let line = s
            .lines()
            .find(|l| l.contains("left-pane-ok"))
            .unwrap_or_default();
        // It must appear to the left of the divider, i.e. in the first pane.
        line.find("left-pane-ok").unwrap_or(usize::MAX) < 25
    });
}

#[test]
fn dragging_a_tile_by_its_title_snaps_it_into_another_pane() {
    let mut h = Harness::start(80, 24);
    h.wait_for("the first pane", |s| s.contains('╭'));
    h.send(b"\x14%"); // split right: two panes side by side
    h.wait_for("a second pane", |s| s.matches('╭').count() > 1);
    let tops_on_row0 = |s: &str| s.lines().next().unwrap_or("").matches('╭').count();
    h.wait_for("two panes on the top row", |s| tops_on_row0(s) == 2);

    // Grab the right-hand pane by its title and drop it on the bottom half of
    // the left one. The columns become stacked rows, so the top row of the
    // screen ends up with a single pane on it.
    h.send(&mouse(0, 41, 0, false));
    h.send(&mouse(32, 20, 20, false));
    h.send(&mouse(0, 20, 20, true));
    h.wait_for("the panes to stack", |s| {
        tops_on_row0(s) == 1 && s.matches('╭').count() == 2
    });
}

#[test]
fn free_mode_lets_a_pane_be_dragged_around() {
    let mut h = Harness::start(80, 24);
    h.wait_for("the first pane", |s| s.contains('╭'));
    h.send(b"\x14%");
    h.wait_for("a second pane", |s| s.matches('╭').count() > 1);

    h.send(b"\x14f"); // float the focused pane
    h.wait_for("free/floating layout", |s| s.matches('╭').count() > 1);

    // Grab the floating pane's top border and drag it down two rows.
    let top_borders = |s: &str| {
        s.lines()
            .enumerate()
            .filter(|(_, l)| l.contains('╭'))
            .map(|(i, _)| i)
            .collect::<Vec<_>>()
    };
    // The float is drawn last, so its top border is the lowest one on screen.
    let before = top_borders(&h.screen());
    let grab = *before.last().unwrap() as u16;
    h.send(&mouse(0, 60, grab, false));
    h.send(&mouse(32, 60, grab + 3, false));
    h.send(&mouse(0, 60, grab + 3, true));
    h.wait_for("the float to move", |s| {
        top_borders(s).last() == Some(&(grab as usize + 3))
    });
}

#[test]
fn the_cli_answers_without_a_terminal() {
    let run = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_ttmux"))
            .args(args)
            .env("TTMUX_CONFIG", "/tmp/ttmux-cli-test.toml")
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
        )
    };

    let (ok, out) = run(&["--version"]);
    assert!(ok && out.starts_with("ttmux 0."), "{out:?}");

    let (ok, out) = run(&["--help"]);
    assert!(ok && out.contains("ctrl+t ,"), "{out:?}");

    let (ok, out) = run(&["--where"]);
    assert!(ok && out.trim() == "/tmp/ttmux-cli-test.toml", "{out:?}");

    // The dumped config is the real default config, and it parses back.
    let (ok, out) = run(&["--print-config"]);
    assert!(ok);
    let parsed: ttmux::config::Config = toml::from_str(&out).unwrap();
    assert_eq!(parsed, ttmux::config::Config::default());

    assert!(!run(&["--nonsense"]).0, "an unknown flag must fail");
}

#[test]
fn quitting_takes_the_shells_and_their_children_with_it() {
    // Quitting drops every `Pane` without going through `close_pane`, so this
    // is the one path where nothing explicitly kills the process group. A
    // backgrounded grandchild is the thing that survives if it regresses.
    let marker = "ttmux-orphan-probe-4311";
    let mut h = Harness::start(80, 24);
    h.wait_for("the first pane", |s| s.contains('╭'));
    // `set -m` is what a shell with job control does, and it is the hard
    // case: the background job gets a process group of its own, so killing
    // the shell's group never reaches it. Linux shells do this on a tty.
    h.send(format!("set -m; sleep 300 & echo {marker}-up\n").as_bytes());
    h.wait_for("the backgrounded child", |s| {
        s.contains(&format!("{marker}-up"))
    });

    h.send(b"\x14Q");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "ttmux did not exit after ctrl+t shift+q"
        );
        if matches!(h._child.try_wait(), Ok(Some(_))) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // The shell reaps its own background job on exit, so give it a moment
    // rather than racing it. The deadline is generous because a loaded CI
    // runner takes seconds to get round to it, and the loop leaves early.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let out = std::process::Command::new("pgrep")
            .args(["-f", "sleep 300"])
            .output()
            .unwrap();
        let hits = String::from_utf8_lossy(&out.stdout);
        if hits.trim().is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "sleep survived ttmux exiting: pids {}",
            hits.trim()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_help_overlay_hides_the_pane_behind_it() {
    // The real end-to-end check for bleed-through: fill the pane with a
    // distinctive word, open help over it, and require that word to be gone
    // from every row the overlay covers.
    let marker = "BLEEDMARKER";
    let mut h = Harness::start(80, 24);
    h.wait_for("the first pane", |s| s.contains('╭'));
    h.send(format!("for i in 1 2 3 4 5 6 7 8 9 10 11 12; do echo {marker}; done\n").as_bytes());
    h.wait_for("the pane to fill", |s| s.matches(marker).count() >= 12);

    h.send(b"\x14?"); // ctrl+t ?
    h.wait_for("the help overlay", |s| s.contains("toggle-float"));

    // Only assert on the rows the overlay actually covers: from its title row
    // to its bottom border. Outside that the pane is supposed to show.
    let lines: Vec<String> = h.screen().lines().map(str::to_string).collect();
    let top = lines
        .iter()
        .position(|l| l.contains("help"))
        .expect("overlay title row");
    let bottom = lines[top..]
        .iter()
        .position(|l| l.contains('╰'))
        .expect("overlay bottom border")
        + top;
    assert!(bottom > top + 2, "overlay looks too small to test");
    for (y, line) in lines.iter().enumerate().take(bottom + 1).skip(top) {
        let covered: String = line.chars().skip(6).take(68).collect();
        assert!(
            !covered.contains(marker),
            "help let the pane through on row {y}: {line:?}"
        );
    }
}

#[test]
fn a_first_run_offers_the_keymap_picker_and_then_gets_out_of_the_way() {
    let mut h = Harness::start_first_run(80, 24);
    h.wait_for("the welcome panel", |s| {
        s.contains("Welcome to ttmux") && s.contains("Modern")
    });
    // It says the choice is not permanent, which is the whole point of
    // showing it before anyone has learned a key.
    h.wait_for("the reassurance", |s| s.contains("changed later"));

    // Pick stock tmux, and the closing screen names that preset's keys.
    h.send(b"2");
    h.wait_for("the closing screen", |s| {
        s.contains("You're ready to go") && s.contains("ctrl+b")
    });

    h.send(b"\r");
    h.wait_for("the picker to get out of the way", |s| {
        !s.contains("You're ready to go") && s.contains('\u{256d}')
    });

    // The preset it wrote is live: ctrl+b % splits, ctrl+t does nothing.
    h.send(b"\x02%");
    h.wait_for("a second pane", |s| s.matches('\u{256d}').count() >= 2);
}

/// A program that positions its cursor with HVP (`CSI … f`) rather than CUP
/// (`CSI … H`) must land where it asked. mpv's `--vo=tct` repositions with `f`
/// once per row of every frame; with those moves dropped the frame paints as
/// one long wrapping stream that scrolls the pane, and two half-frames end up
/// on screen at once.
#[test]
fn a_pane_honours_hvp_cursor_positioning() {
    let mut h = Harness::start(80, 24);
    h.wait_for("the first pane", |s| s.contains('\u{256d}'));

    // What tct does, minus the colour: home, then one absolute move per row.
    // Rows 3..=6 of the pane, each with a marker wide enough that a wrapping
    // stream would visibly run them together.
    let x = "x".repeat(68);
    h.send(
        format!(
            "clear; for i in 3 4 5 6; do printf '\\033[%d;3fR%d{x}' $i $i; done; \
             printf '\\033[9;3fDO''NE'\r"
        )
        .as_bytes(),
    );
    h.wait_for("the last write", |s| s.contains("DONE"));

    // The pane's interior starts at (1, 1) of the screen and the escape's row
    // is 1-based, so `CSI N;3f` lands on screen row N and screen column 3.
    let lines: Vec<String> = h.screen().lines().map(str::to_string).collect();
    // Columns, not byte offsets: the border glyph is three bytes wide.
    let at = |row: &str, col: usize, s: &str| {
        row.chars().skip(col).take(s.chars().count()).eq(s.chars())
    };
    for (i, line) in lines.iter().enumerate().take(7).skip(3) {
        assert!(
            at(line, 3, &format!("R{i}")),
            "row {i} did not land at column 3:\n{}",
            h.screen()
        );
    }
    assert!(at(&lines[9], 3, "DONE"), "{}", h.screen());
    // Nothing wrapped into the rows in between.
    assert!(
        lines[7..9]
            .iter()
            .all(|l| l.trim_matches(|c| c == '\u{2502}' || c == ' ').is_empty()),
        "content wrapped past its row:\n{}",
        h.screen()
    );
}
