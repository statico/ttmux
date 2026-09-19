//! End-to-end attach/detach against a real daemon.
//!
//! Every test gets its own `$TTMUX_SOCKET` and its own config under a
//! `tempfile` directory, and kills its server in `Drop`: a test that leaked a
//! daemon onto the machine, or touched the user's real sessions, would be a
//! defect in the test.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use ttmux::proto::{self, ClientMsg, ServerMsg};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Session {
    _dir: TempDir,
    sock: PathBuf,
}

impl Session {
    fn start() -> Session {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("s.sock");
        // A config path that does not exist: the app falls back to defaults
        // rather than reading whatever the developer running the tests has.
        let cfg = dir.path().join("nonexistent.toml");
        let status = Command::new(env!("CARGO_BIN_EXE_ttmux"))
            .args(["new", "-d", "-s", "test"])
            .env("TTMUX_SOCKET", &sock)
            .env("TTMUX_CONFIG", &cfg)
            .status()
            .unwrap();
        assert!(status.success(), "ttmux new -d failed");
        let s = Session { _dir: dir, sock };
        assert!(s.live(), "server did not come up");
        s
    }

    fn live(&self) -> bool {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if proto::is_live(&self.sock) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn attach(&self, cols: u16, rows: u16) -> Client {
        let sock = UnixStream::connect(&self.sock).unwrap();
        sock.set_read_timeout(Some(TIMEOUT)).unwrap();
        let mut c = Client { sock };
        c.send(&ClientMsg::Hello {
            proto: proto::PROTOCOL,
            cols,
            rows,
            term: "xterm-256color".into(),
            colours: None,
            env: vec![],
        });
        match c.recv() {
            Some(ServerMsg::Welcome { proto: v, .. }) => assert_eq!(v, proto::PROTOCOL),
            other => panic!("expected Welcome, got {other:?}"),
        }
        c
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let Ok(mut sock) = UnixStream::connect(&self.sock) else {
            return;
        };
        let _ = sock.set_read_timeout(Some(TIMEOUT));
        let _ = proto::write_msg(&mut sock, &ClientMsg::KillServer);
        while let Ok(Some(_)) = proto::read_msg::<_, ServerMsg>(&mut sock) {}
    }
}

struct Client {
    sock: UnixStream,
}

impl Client {
    fn send(&mut self, msg: &ClientMsg) {
        proto::write_msg(&mut self.sock, msg).unwrap();
    }

    /// `None` at a clean EOF; panics if the server went quiet instead.
    fn recv(&mut self) -> Option<ServerMsg> {
        match proto::read_msg::<_, ServerMsg>(&mut self.sock) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => panic!("timed out waiting"),
            Err(e) => panic!("read: {e}"),
        }
    }

    /// Read until a frame paints something at or past `x`, which is how a
    /// test sees the session's width without a terminal.
    fn wait_for_column(&mut self, x: u16) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if let Some(ServerMsg::Draw(cells)) = self.recv() {
                if cells.iter().any(|c| c.x >= x) {
                    return;
                }
            }
        }
        panic!("nothing was ever drawn at column {x}");
    }

    fn wait_for_clear(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if matches!(self.recv(), Some(ServerMsg::Clear)) {
                return;
            }
        }
        panic!("no clear");
    }
}

#[test]
fn attach_detach_reattach_keeps_the_session() {
    let s = Session::start();

    let mut a = s.attach(100, 40);
    a.wait_for_column(99);
    a.send(&ClientMsg::Detach);
    // The server says goodbye to this one client and closes it.
    loop {
        match a.recv() {
            Some(ServerMsg::Bye(_)) | None => break,
            Some(_) => {}
        }
    }
    drop(a);

    // The session outlives its last client: that is the whole point.
    assert!(proto::is_live(&s.sock));
    let mut b = s.attach(100, 40);
    b.wait_for_column(99);
}

#[test]
fn the_session_is_as_wide_as_its_narrowest_client() {
    let s = Session::start();
    let mut big = s.attach(100, 40);
    big.wait_for_column(99);

    let mut small = s.attach(60, 20);
    small.wait_for_column(59);

    // The big client is now painting a 60x20 session, not a 100x40 one. The
    // resize clears, and the frame after it is the whole screen at the new
    // size, so one frame is the whole assertion.
    big.wait_for_clear();
    let mut painted = 0;
    while painted == 0 {
        if let Some(ServerMsg::Draw(cells)) = big.recv() {
            for c in &cells {
                assert!(
                    c.x < 60 && c.y < 20,
                    "painted {},{} outside 60x20",
                    c.x,
                    c.y
                );
            }
            painted += cells.len();
        }
    }
    assert!(
        painted > 20,
        "only {painted} cells painted after the shrink"
    );

    // ...and losing the small client gives the width back.
    small.send(&ClientMsg::Detach);
    drop(small);
    big.wait_for_column(99);
}

#[test]
fn kill_server_takes_the_socket_with_it() {
    let s = Session::start();
    let mut a = s.attach(80, 24);
    a.send(&ClientMsg::KillServer);
    let deadline = Instant::now() + TIMEOUT;
    while proto::is_live(&s.sock) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!proto::is_live(&s.sock), "server outlived kill-server");
    assert!(!s.sock.exists(), "socket file was left behind");
}
