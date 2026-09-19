//! `ttmux upgrade`: the session moves to a new server and the shells do not
//! notice.
//!
//! The two things that must hold are that the shell is the *same process*
//! (its variables are still set) and that its screen came with it.

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

    #[track_caller]
    fn upgrade(&self) {
        let out = self.run(&["upgrade"]);
        assert!(
            out.status.success(),
            "upgrade failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn pid(&self) -> u32 {
        ttmux::migrate::describe(&self.sock)
            .expect("nothing serving")
            .1
    }

    /// The pane's text, history and all.
    fn capture(&self) -> String {
        String::from_utf8_lossy(&self.run(&["capture-pane", "-S", "-"]).stdout).to_string()
    }

    /// Keep looking until `needle` is in the pane. Everything here waits on a
    /// shell running somewhere else, so nothing is ever "done" on a timer.
    #[track_caller]
    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.capture().contains(needle) {
            if Instant::now() > deadline {
                let out = self.run(&["capture-pane", "-S", "-"]);
                panic!(
                    "never saw {needle:?}; the pane had:\n{}\n{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
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
    // Quoted apart, so the echoed command line cannot match the output.
    s.keys("MARKER=survivor; echo screen-was''-here");
    s.wait_for("screen-was-here");
    let before = s.pid();

    s.upgrade();
    let after = s.pid();
    assert_ne!(before, after, "the same server is still serving");

    s.wait_for("screen-was-here");

    // Twice: an adopted session has to be handed on again, which is the
    // path a second `ttmux upgrade` takes.
    s.upgrade();
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

    s.upgrade();

    let after = String::from_utf8_lossy(&s.run(&["list-panes", "-a"]).stdout).to_string();
    assert_eq!(before, after, "the layout changed across the handover");
}

#[test]
fn an_adopted_pane_still_closes_when_its_shell_exits() {
    let s = Session::start();
    s.run(&["split-window"]);
    s.upgrade();

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

#[test]
fn upgrade_keeps_the_scrollback_and_the_screen_where_they_were() {
    let s = Session::start();
    s.keys("seq 1 60; echo filled''-up");
    s.wait_for("filled-up");
    let before = s.capture();
    assert!(before.contains("\n1\n"), "no scrollback to test: {before}");

    // Twice: a replay that is off by a row, or adds one, shows on the
    // second pass if it hid on the first.
    for _ in 0..2 {
        s.upgrade();
        assert_eq!(
            before,
            s.capture(),
            "the pane's text moved across the handover"
        );
    }
}

#[test]
fn a_full_screen_program_keeps_the_history_under_it() {
    let s = Session::start();
    s.keys("seq 1 60; printf '\\033[?1049hin-the''-alt-screen'; read x; printf '\\033[?1049l'");
    s.wait_for("in-the-alt-screen");
    s.upgrade();
    s.wait_for("in-the-alt-screen");
    s.keys("");
    let deadline = Instant::now() + TIMEOUT;
    while !s.capture().contains("\n1\n") {
        assert!(
            Instant::now() < deadline,
            "the history is gone: {}",
            s.capture()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_view_scrolled_back_stays_put_when_new_output_arrives() {
    let s = Session::start();
    // Output that comes on its own: a key would snap the view back down.
    s.keys("seq 1 200; sleep 3; echo lat''er");
    s.wait_for("\n200\n");
    s.upgrade();
    assert!(s.run(&["run", "scroll-up"]).status.success());
    let view = || String::from_utf8_lossy(&s.run(&["capture-pane"]).stdout).to_string();
    let before = view();
    s.wait_for("later");
    assert_eq!(before, view(), "the view jumped on the first new output");
}

#[test]
fn a_pinned_footer_stays_on_the_bottom_row() {
    let s = Session::start();
    s.keys(
        "R=$(stty size | cut -d' ' -f1); seq 1 30; \
         printf \"\\033[1;$((R-1))r\\033[$R;1HFOOT\"\"ER\\033[$((R-1));1H\"; read x; echo NEW; read y",
    );
    s.wait_for("FOOTER");
    s.upgrade();
    s.keys("");
    s.wait_for("NEW");
    let screen = String::from_utf8_lossy(&s.run(&["capture-pane"]).stdout).to_string();
    assert_eq!(screen.lines().last(), Some("FOOTER"), "{screen}");
}
