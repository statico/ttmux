//! `ttmux upgrade`: the session moves to a new server and the shells do not
//! notice.
//!
//! The two things that must hold are that the shell is the *same process*
//! (its variables are still set) and that its screen came with it.

use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use ttmux::proto::{self, ClientMsg, ServerMsg};

const TIMEOUT: Duration = Duration::from_secs(20);

struct Session {
    _dir: TempDir,
    sock: PathBuf,
    cfg: PathBuf,
}

impl Session {
    fn start() -> Session {
        let dir = tempfile::tempdir().unwrap();
        let s = Session {
            sock: dir.path().join("s.sock"),
            cfg: dir.path().join("nonexistent.toml"),
            _dir: dir,
        };
        assert!(s.run(&["new", "-d", "-s", "test"]).status.success());
        let deadline = Instant::now() + TIMEOUT;
        while !proto::is_live(&s.sock) {
            assert!(Instant::now() < deadline, "server did not come up");
            std::thread::sleep(Duration::from_millis(20));
        }
        s
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_ttmux"))
            .args(args)
            .env("TTMUX_SOCKET", &self.sock)
            .env("TTMUX_CONFIG", &self.cfg)
            .env_remove("TTMUX")
            .output()
            .unwrap()
    }

    fn keys(&self, line: &str) {
        assert!(self.run(&["send-keys", line, "Enter"]).status.success());
    }

    fn pid(&self) -> u32 {
        ttmux::migrate::describe(&self.sock)
            .expect("nothing serving")
            .1
    }

    /// Keep looking until `needle` is on the screen. Everything here waits on
    /// a shell running somewhere else, so nothing is ever "done" on a timer.
    fn wait_for(&self, needle: &str) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let screen = self.screen();
            if screen.contains(needle) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "never saw {needle:?}; screen was:\n{screen}"
            );
        }
    }

    /// Attach, collect frames until the screen stops changing, and flatten it
    /// to one string -- enough to ask "is that text still there?".
    fn screen(&self) -> String {
        let mut sock = UnixStream::connect(&self.sock).unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        proto::write_msg(
            &mut sock,
            &ClientMsg::Hello {
                proto: proto::PROTOCOL,
                cols: 80,
                rows: 24,
                term: "xterm-256color".into(),
                colours: None,
                env: vec![],
            },
        )
        .unwrap();
        let mut cells: BTreeMap<(u16, u16), String> = BTreeMap::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            match proto::read_msg::<_, ServerMsg>(&mut sock) {
                Ok(Some(ServerMsg::Draw(cs))) => {
                    for c in cs {
                        cells.insert((c.y, c.x), c.symbol);
                    }
                }
                Ok(Some(ServerMsg::Clear)) => cells.clear(),
                Ok(Some(_)) => {}
                _ => break,
            }
        }
        let mut out = String::new();
        let mut row = None;
        for ((y, _), sym) in cells {
            if row != Some(y) {
                out.push('\n');
                row = Some(y);
            }
            out.push_str(&sym);
        }
        out
    }
}

impl Drop for Session {
    /// Down the socket, not `ttmux kill-server`: that verb walks the real
    /// socket directory, so it would miss this test's server and hit the
    /// developer's own sessions.
    fn drop(&mut self) {
        let Ok(mut sock) = UnixStream::connect(&self.sock) else {
            return;
        };
        let _ = sock.set_read_timeout(Some(TIMEOUT));
        let _ = proto::write_msg(&mut sock, &ClientMsg::KillServer);
        while let Ok(Some(_)) = proto::read_msg::<_, ServerMsg>(&mut sock) {}
    }
}

#[test]
fn upgrade_keeps_the_shell_and_its_screen() {
    let s = Session::start();
    s.keys("MARKER=survivor; echo screen-was-here");
    s.wait_for("screen-was-here");
    let before = s.pid();

    let out = s.run(&["upgrade"]);
    assert!(
        out.status.success(),
        "upgrade failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = s.pid();
    assert_ne!(before, after, "the same server is still serving");

    s.wait_for("screen-was-here");

    // Twice: an adopted session has to be handed on again, which is the
    // path a second `ttmux upgrade` takes.
    let out = s.run(&["upgrade"]);
    assert!(
        out.status.success(),
        "second upgrade failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_ne!(after, s.pid());

    // The same shell, not a new one: only it knows $MARKER.
    s.keys("echo \"[$MARKER]\"");
    s.wait_for("[survivor]");
}

#[test]
fn upgrade_keeps_every_pane_and_window() {
    let s = Session::start();
    s.run(&["split-window"]);
    s.run(&["new-window"]);
    let before = String::from_utf8_lossy(&s.run(&["list-panes", "-a"]).stdout).to_string();
    assert_eq!(before.lines().count(), 3, "expected three panes: {before}");

    let out = s.run(&["upgrade"]);
    assert!(
        out.status.success(),
        "upgrade failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let after = String::from_utf8_lossy(&s.run(&["list-panes", "-a"]).stdout).to_string();
    assert_eq!(before, after, "the layout changed across the handover");
}

#[test]
fn an_adopted_pane_still_closes_when_its_shell_exits() {
    let s = Session::start();
    s.run(&["split-window"]);
    assert!(s.run(&["upgrade"]).status.success());

    // The new server never forked this shell, so it cannot `wait` for it;
    // noticing it has gone is `AdoptedChild`'s whole job.
    s.keys("exit");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let panes = String::from_utf8_lossy(&s.run(&["list-panes", "-a"]).stdout).to_string();
        if panes.lines().count() == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the adopted pane never closed: {panes}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
