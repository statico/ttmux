//! Wire protocol between the ttmux client and the server that owns the ptys.
//!
//! Length-prefixed JSON over a unix socket. JSON because the frames are tiny
//! next to the pty traffic they describe, and a readable wire is worth more
//! here than the bytes it costs.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use ratatui::style::Style;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Bumped whenever the wire format changes. It is part of the socket path,
/// so a new binary starts its own server and an old client keeps talking to
/// the old one -- which is how upgrading does not cost you your sessions.
pub const PROTOCOL: u32 = 1;

/// Refuse a frame larger than this rather than allocating what the peer asked
/// for. A whole 200x60 repaint is well under a megabyte of JSON.
pub const MAX_FRAME: u32 = 4 << 20;

/// One painted cell, as ratatui's `Backend::draw` hands them over.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireCell {
    pub x: u16,
    pub y: u16,
    pub symbol: String,
    pub style: Style,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello {
        proto: u32,
        cols: u16,
        rows: u16,
        term: String,
    },
    Input(crossterm::event::Event),
    Detach,
    KillServer,
    /// One scripting command and its arguments, already split by the shell.
    /// A command is a whole connection: no hello, no view, one reply.
    Command(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome {
        proto: u32,
        version: String,
    },
    /// Repaint these cells. Empty is legal.
    Draw(Vec<WireCell>),
    /// Blank the whole screen; sent on attach so a fresh client repaints.
    Clear,
    Cursor(Option<(u16, u16)>),
    /// Write these bytes to the terminal verbatim: inline-image replays.
    Passthrough(Vec<u8>),
    /// Leave, and say why. The client restores the terminal and exits.
    Bye(String),
    Error(String),
    /// The answer to a `Command`. `ok` is the process exit status the client
    /// turns it into; `text` is what it prints.
    Reply {
        ok: bool,
        text: String,
    },
}

// ------------------------------------------------------------------ framing

/// 4-byte little-endian length, then JSON.
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let len = u32::try_from(body.len())
        .ok()
        .filter(|n| *n <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// `Ok(None)` only at a clean frame boundary; anything short of a whole frame
/// is an error, so the caller can tell "peer detached" from "peer died".
pub fn read_msg<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut hdr = [0u8; 4];
    if !read_full(r, &mut hdr)? {
        return Ok(None);
    }
    let len = u32::from_le_bytes(hdr);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds cap"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    if !read_full(r, &mut body)? {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated frame",
        ));
    }
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(io::Error::other)
}

/// False only when nothing at all was read; a partial fill is `UnexpectedEof`.
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) if n == 0 => return Ok(false),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated frame",
                ))
            }
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

// ------------------------------------------------------------------- paths

/// Directory holding this user's sockets, created mode 0700: it hands out
/// full shell access, so the permissions are the trust boundary.
pub fn socket_dir() -> io::Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("TMPDIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let dir = base.join(format!("ttmux-{}", unsafe { libc::getuid() }));
    if !dir.exists() {
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
    }
    // The directory holds a socket that hands out shell access, so on a
    // shared /tmp someone else owning it is a hijack, not an inconvenience.
    // Tightening the mode does not help if the owner is wrong.
    let meta = fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a directory you own", dir.display()),
        ));
    }
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// `$TTMUX_SOCKET` wins outright, as an absolute path to the socket itself.
pub fn socket_path(session: &str) -> io::Result<PathBuf> {
    if let Some(p) = std::env::var_os("TTMUX_SOCKET") {
        return Ok(PathBuf::from(p));
    }
    if session.is_empty()
        || session.contains('/')
        || session.contains('\0')
        || session.contains("..")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("bad session name {session:?}"),
        ));
    }
    Ok(socket_dir()?.join(format!("{session}-{PROTOCOL}")))
}

/// Every socket in the directory speaking *this* `PROTOCOL`, live or stale.
pub fn list_sessions() -> Vec<(String, PathBuf)> {
    let suffix = format!("-{PROTOCOL}");
    let Ok(dir) = socket_dir() else {
        return vec![];
    };
    let Ok(rd) = fs::read_dir(&dir) else {
        return vec![];
    };
    let mut out: Vec<(String, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let session = name.strip_suffix(&suffix)?;
            Some((session.to_string(), e.path()))
        })
        .collect();
    out.sort();
    out
}

/// Can anything still be reached at `path`? A crashed server leaves the file.
pub fn is_live(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

/// Remove the sockets of servers that are gone.
pub fn cleanup_stale() {
    for (_, path) in list_sessions() {
        if !is_live(&path) {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent};
    use ratatui::style::{Color, Modifier};

    fn cell() -> WireCell {
        WireCell {
            x: 3,
            y: 4,
            symbol: "é".into(),
            style: Style::default()
                .fg(Color::Rgb(1, 2, 3))
                .bg(Color::Indexed(9))
                .add_modifier(Modifier::BOLD),
        }
    }

    fn roundtrip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(msgs: &[T]) {
        let mut buf = vec![];
        for m in msgs {
            write_msg(&mut buf, m).unwrap();
        }
        let mut r = &buf[..];
        for m in msgs {
            assert_eq!(read_msg::<_, T>(&mut r).unwrap().as_ref(), Some(m));
        }
        // Clean EOF after the last whole frame.
        assert!(read_msg::<_, T>(&mut r).unwrap().is_none());
    }

    #[test]
    fn every_variant_roundtrips() {
        roundtrip(&[
            ClientMsg::Hello {
                proto: PROTOCOL,
                cols: 80,
                rows: 24,
                term: "xterm-256color".into(),
            },
            ClientMsg::Input(Event::Key(KeyEvent::from(KeyCode::Char('q')))),
            ClientMsg::Input(Event::Resize(100, 40)),
            ClientMsg::Detach,
            ClientMsg::KillServer,
        ]);
        roundtrip(&[
            ServerMsg::Welcome {
                proto: PROTOCOL,
                version: "0.1.0".into(),
            },
            ServerMsg::Draw(vec![cell()]),
            ServerMsg::Draw(vec![]),
            ServerMsg::Clear,
            ServerMsg::Cursor(Some((7, 8))),
            ServerMsg::Cursor(None),
            ServerMsg::Passthrough(vec![0x1b, b'_', 7]),
            ServerMsg::Bye("detached".into()),
            ServerMsg::Error("nope".into()),
        ]);
    }

    #[test]
    fn two_messages_read_back_in_order() {
        let mut buf = vec![];
        write_msg(&mut buf, &ServerMsg::Clear).unwrap();
        write_msg(&mut buf, &ServerMsg::Bye("bye".into())).unwrap();
        let mut r = &buf[..];
        assert_eq!(
            read_msg::<_, ServerMsg>(&mut r).unwrap(),
            Some(ServerMsg::Clear)
        );
        assert_eq!(
            read_msg::<_, ServerMsg>(&mut r).unwrap(),
            Some(ServerMsg::Bye("bye".into()))
        );
        assert_eq!(read_msg::<_, ServerMsg>(&mut r).unwrap(), None);
    }

    #[test]
    fn truncation_is_an_error_but_eof_is_not() {
        assert!(read_msg::<_, ServerMsg>(&mut &[][..]).unwrap().is_none());
        // Half a length prefix.
        assert!(read_msg::<_, ServerMsg>(&mut &[1u8, 0][..]).is_err());
        // Whole prefix, half a body.
        let mut buf = vec![];
        write_msg(&mut buf, &ServerMsg::Bye("a longer reason".into())).unwrap();
        buf.truncate(buf.len() - 3);
        assert!(read_msg::<_, ServerMsg>(&mut &buf[..]).is_err());
    }

    #[test]
    fn over_cap_length_errors_without_allocating() {
        let hdr = (MAX_FRAME + 1).to_le_bytes();
        let err = read_msg::<_, ServerMsg>(&mut &hdr[..]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn socket_paths() {
        std::env::remove_var("TTMUX_SOCKET");
        let p = socket_path("work").unwrap();
        assert_eq!(
            p.file_name().unwrap().to_str().unwrap(),
            format!("work-{PROTOCOL}")
        );
        assert_eq!(
            fs::metadata(p.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(socket_path("a/b").is_err());
        assert!(socket_path("../escape").is_err());

        std::env::set_var("TTMUX_SOCKET", "/tmp/ttmux-test.sock");
        // The override wins even over a name that would otherwise be rejected.
        assert_eq!(
            socket_path("a/b").unwrap(),
            PathBuf::from("/tmp/ttmux-test.sock")
        );
        std::env::remove_var("TTMUX_SOCKET");
    }

    #[test]
    fn is_live_is_false_for_a_plain_file() {
        let f = tempfile::NamedTempFile::new().unwrap();
        assert!(!is_live(f.path()));
        assert!(!is_live(Path::new("/nonexistent/ttmux.sock")));
    }
}
