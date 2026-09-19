//! Handing a live session to a new server process.
//!
//! A server is stamped at birth with the identity of the terminal that
//! started it: its macOS audit session, its TCC responsible process, and the
//! environment of that first client (`SSH_AUTH_SOCK` and friends). None of
//! that can be changed afterwards -- see [`crate::mac`] -- so the only way to
//! give a session a fresh identity is to run it in a process that was spawned
//! by a terminal that is still there.
//!
//! That is also, exactly, what upgrading to a new build needs. So it is one
//! mechanism:
//!
//! 1. `ttmux upgrade` forks a new server *from the client*, which is running
//!    in the live terminal and so has its identity and environment.
//! 2. The new server binds a socket beside the old one, connects to the old
//!    server and asks for the session.
//! 3. The old server sends a [`Snapshot`] -- tabs, layouts, pane screens --
//!    and then each pane's pty master over `SCM_RIGHTS`.
//! 4. The new server renames its socket over the old one and says so; the old
//!    server calls `_exit`, which skips every destructor and so leaves the
//!    shells alone. The pty masters stay open throughout, in one process or
//!    the other, so no child ever sees a hangup.
//!
//! The shells themselves, their jobs, their working directories, what is on
//! their screens and their scrollback all survive. What does not: the
//! *formatting* of the scrollback (it is replayed as plain text), inline
//! images, whatever a full-screen program had underneath it on the primary
//! grid, and the exit status of a pane that dies later -- the new server is
//! not its parent, so it watches the pid instead of reaping it.
//!
//! A handover that fails at any point before the new server takes the socket
//! over is a no-op: the old server keeps the session and says why.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::layout::{Layout, PaneId};
use crate::proto::{self, ClientMsg, ServerMsg};

/// How long the two servers give each other. Generous, because the new
/// server replays every pane's scrollback through a vt100 parser before it
/// can answer, and a session with a lot of history is not quick. It still
/// has to end: the old server must never be left wedged holding a session
/// nobody can reach.
const TIMEOUT: Duration = Duration::from_secs(120);

// ------------------------------------------------------------- the payload

/// Everything about a session that is not a file descriptor.
///
/// This is the one wire format that has to survive a version change, because
/// the whole point is that an old server writes it and a *newer* one reads
/// it. Every field is optional-by-default on the way in, so added and
/// removed fields cost decoration rather than panes.
///
/// A reader that cannot parse it at all refuses before a single descriptor
/// has moved, and the old server keeps the session: a failed upgrade is
/// always a no-op, never a lost shell.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// The version that wrote it, for the log and for error messages.
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub tab: usize,
    #[serde(default)]
    pub next_id: PaneId,
    /// The size the session was being laid out at, so the new server does not
    /// squeeze every pane through 80x24 before the first client attaches.
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub tabs: Vec<TabSnap>,
    /// One per pty master that follows, in the same order.
    #[serde(default)]
    pub panes: Vec<PaneSnap>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TabSnap {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub renamed: bool,
    #[serde(default)]
    pub focus: PaneId,
    pub layout: Layout,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PaneSnap {
    pub id: PaneId,
    /// The shell's pid, which is also its process group and session id. The
    /// new server is not its parent, so this is all it has to kill it with.
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub title_override: Option<String>,
    /// Escape codes that reproduce what is on the pane's screen, plus
    /// anything the old server had read but not yet drawn.
    #[serde(default, with = "crate::proto::text_bytes")]
    pub replay: Vec<u8>,
}

// --------------------------------------------------------- fd passing

/// Big enough for one `SCM_RIGHTS` header and one fd on both platforms, and
/// aligned like a `cmsghdr` because that is what the kernel writes into it.
#[repr(C)]
union CmsgBuf {
    _align: libc::cmsghdr,
    bytes: [u8; 64],
}

/// One byte of payload and one descriptor. The byte matters: ancillary data
/// rides with real data, and a zero-length send would carry nothing.
fn send_fd(sock: &UnixStream, fd: RawFd) -> io::Result<()> {
    let mut byte = [b'f'];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut cmsg = CmsgBuf { bytes: [0; 64] };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    unsafe {
        msg.msg_control = cmsg.bytes.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(4) as _;
        let hdr = libc::CMSG_FIRSTHDR(&msg);
        (*hdr).cmsg_level = libc::SOL_SOCKET;
        (*hdr).cmsg_type = libc::SCM_RIGHTS;
        (*hdr).cmsg_len = libc::CMSG_LEN(4) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(hdr).cast::<RawFd>(), fd);
        loop {
            match libc::sendmsg(sock.as_raw_fd(), &msg, 0) {
                1 => break,
                // A signal here would otherwise cost one pane its pty, and
                // the fds after it their place in the queue.
                n if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
                n if n < 0 => return Err(io::Error::last_os_error()),
                _ => return Err(io::Error::other("pty was only partly sent")),
            }
        }
    }
    Ok(())
}

/// The other half. Reads exactly one byte, so descriptors arrive one to a
/// message and in the order they were sent.
fn recv_fd(sock: &UnixStream) -> io::Result<OwnedFd> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut cmsg = CmsgBuf { bytes: [0; 64] };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    unsafe {
        msg.msg_control = cmsg.bytes.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(4) as _;
        let mut n = libc::recvmsg(sock.as_raw_fd(), &mut msg, 0);
        while n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            n = libc::recvmsg(sock.as_raw_fd(), &mut msg, 0);
        }
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "handover ended before the ptys arrived",
            ));
        }
        let hdr = libc::CMSG_FIRSTHDR(&msg);
        if hdr.is_null()
            || (*hdr).cmsg_level != libc::SOL_SOCKET
            || (*hdr).cmsg_type != libc::SCM_RIGHTS
            || (*hdr).cmsg_len as usize != libc::CMSG_LEN(4) as usize
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handover message carried no pty",
            ));
        }
        let fd = std::ptr::read_unaligned(libc::CMSG_DATA(hdr).cast::<RawFd>());
        // macOS has no MSG_CMSG_CLOEXEC, so close-on-exec is set here: a pane
        // must never inherit another pane's master.
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

// ------------------------------------------------------- the two halves

/// The old server's side: send the session, then wait to be told it landed.
///
/// Returns once the new server has the lot *and* has taken over the socket.
/// The caller must then leave without running any destructor, because every
/// pane here is now someone else's.
pub fn give(sock: &mut UnixStream, snap: &Snapshot, fds: &[OwnedFd]) -> Result<()> {
    if snap.panes.len() != fds.len() {
        bail!(
            "snapshot describes {} panes but has {} ptys",
            snap.panes.len(),
            fds.len()
        );
    }
    let json = serde_json::to_string(snap).context("serialise session")?;
    proto::write_msg(sock, &ServerMsg::Handover(json)).context("send session")?;
    for fd in fds {
        send_fd(sock, fd.as_raw_fd()).context("send pty")?;
    }
    sock.set_read_timeout(Some(TIMEOUT)).ok();
    match proto::read_msg::<_, ClientMsg>(sock) {
        Ok(Some(ClientMsg::Adopted)) => Ok(()),
        Ok(other) => bail!("new server said {other:?} instead of taking the session"),
        Err(e) => Err(e).context("waiting for the new server"),
    }
}

/// The new server's side: ask for the session and take it.
///
/// The socket comes back with it: the old server does not let go until
/// [`confirm`] is called, which must be after this server's own socket is in
/// place, or a client attaching in between would find nothing.
pub fn take(from: &Path) -> Result<(Snapshot, Vec<OwnedFd>, UnixStream)> {
    let mut sock = UnixStream::connect(from)
        .with_context(|| format!("connect to the running server at {}", from.display()))?;
    sock.set_read_timeout(Some(TIMEOUT))?;
    sock.set_write_timeout(Some(TIMEOUT))?;
    proto::write_msg(&mut sock, &ClientMsg::Adopt)?;
    let snap: Snapshot = match proto::read_msg::<_, ServerMsg>(&mut sock)? {
        Some(ServerMsg::Handover(json)) => serde_json::from_str(&json)
            .context("the running server sent a session this build cannot read")?,
        Some(ServerMsg::Error(e)) => bail!("the running server refused: {e}"),
        other => bail!("the running server said {other:?} instead of handing over"),
    };
    let mut fds = Vec::with_capacity(snap.panes.len());
    for pane in &snap.panes {
        fds.push(recv_fd(&sock).with_context(|| format!("receiving pane {}", pane.id))?);
    }
    Ok((snap, fds, sock))
}

/// Tell the old server the session is safely here. It exits on this.
pub fn confirm(sock: &mut UnixStream) -> Result<()> {
    proto::write_msg(sock, &ClientMsg::Adopted)?;
    Ok(())
}

// ------------------------------------------------------------- the verb

/// `ttmux upgrade`: move `session` into a server forked from *this* process.
///
/// Forked, not exec'd: a fork inherits this terminal's macOS identity and
/// environment, which is the entire point on macOS, and this process is
/// already the new binary because it is the one the user just ran.
pub fn upgrade(session: &str) -> Result<()> {
    let path = proto::socket_path(session)?;
    if !proto::is_live(&path) {
        bail!("no server for session {session:?}");
    }
    let before = describe(&path)
        .map(|(_, pid)| pid)
        .context("the running server did not answer; not touching it")?;
    crate::server::spawn_adopting(session, &path)?;

    let deadline = Instant::now() + TIMEOUT;
    loop {
        // The new server renames its socket over this path, so "something is
        // live here" proves nothing: the handover is done when the pid
        // answering has changed.
        if let Some((version, pid)) = describe(&path) {
            if pid != before {
                println!("session {session:?} is now served by ttmux {version} (pid {pid})");
                return Ok(());
            }
        }
        if Instant::now() > deadline {
            bail!("the new server did not take over; the old one is still running");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The version and pid of whatever is answering at `path`, or `None` if
/// nothing is.
pub fn describe(path: &Path) -> Option<(String, u32)> {
    let mut sock = UnixStream::connect(path).ok()?;
    sock.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    sock.set_write_timeout(Some(Duration::from_secs(2))).ok()?;
    proto::write_msg(&mut sock, &ClientMsg::Version).ok()?;
    match proto::read_msg::<_, ServerMsg>(&mut sock) {
        Ok(Some(ServerMsg::Welcome { version, pid, .. })) => Some((version, pid)),
        _ => None,
    }
}

/// Where a server being adopted puts its socket until it is ready to be the
/// session. Unique per client, so two upgrades at once cannot collide.
pub fn pending_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".new.{}", std::process::id()));
    PathBuf::from(name)
}
