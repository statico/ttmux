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
    _dir: tempfile::TempDir,
}

impl Harness {
    fn start(cols: u16, rows: u16) -> Harness {
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
        cmd.env("TTMUX_CONFIG", dir.path().join("ttmux.toml"));
        cmd.env("TTMUX_SESSION", "test");
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

    h.send(b"\x01%"); // ctrl+a %
    h.wait_for("a second pane", |s| s.matches('╭').count() > before);

    // Both panes are live shells, and typing goes to the new one.
    h.send(b"echo new''-pane-ok\r");
    h.wait_for("output in the new pane", |s| s.contains("new-pane-ok"));
}

#[test]
fn the_help_overlay_opens_and_closes() {
    let mut h = Harness::start(80, 24);
    h.wait_for("the first pane", |s| s.contains('╭'));

    h.send(b"\x01?"); // ctrl+a ?
    h.wait_for("the help overlay", |s| {
        s.contains("help") && s.contains("split")
    });

    h.send(b"\x1b"); // any key closes it
    h.wait_for("help to close", |s| !s.contains("toggle-layout-mode"));
}

#[test]
fn the_settings_overlay_opens() {
    let mut h = Harness::start(80, 24);
    h.wait_for("the first pane", |s| s.contains('╭'));

    h.send(b"\x01s"); // ctrl+a s
    h.wait_for("the settings panel", |s| {
        s.contains("Appearance") && s.contains("General")
    });
}

#[test]
fn quitting_exits_cleanly() {
    let mut h = Harness::start(80, 24);
    h.wait_for("the first pane", |s| s.contains('╭'));

    h.send(b"\x01q"); // ctrl+a q
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(Some(status)) = h._child.try_wait() {
            assert!(status.success(), "ttmux exited with {status:?}");
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("ttmux did not exit after ctrl+a q");
}

/// Print a real session. `cargo test --test smoke -- --ignored --nocapture`.
#[test]
#[ignore = "visual aid, not an assertion"]
fn snapshot() {
    let mut h = Harness::start(100, 28);
    h.wait_for("the first pane", |s| s.contains('╭'));
    h.send(b"\x01%");
    h.wait_for("a second pane", |s| s.matches('╭').count() > 1);
    h.send(b"\x01\"");
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
    h.send(b"\x01%"); // split right; focus moves to the right-hand pane
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
fn free_mode_lets_a_pane_be_dragged_around() {
    let mut h = Harness::start(80, 24);
    h.wait_for("the first pane", |s| s.contains('╭'));
    h.send(b"\x01%");
    h.wait_for("a second pane", |s| s.matches('╭').count() > 1);

    h.send(b"\x01f"); // float the focused pane
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
    assert!(ok && out.contains("ctrl+a s"), "{out:?}");

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
    h.send(format!("sleep 300 & echo {marker}-up\n").as_bytes());
    h.wait_for("the backgrounded child", |s| {
        s.contains(&format!("{marker}-up"))
    });

    h.send(b"\x01q");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "ttmux did not exit after ctrl+a q"
        );
        if matches!(h._child.try_wait(), Ok(Some(_))) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // The shell reaps its own background job on exit, so give it a moment
    // rather than racing it.
    let deadline = Instant::now() + Duration::from_secs(5);
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
