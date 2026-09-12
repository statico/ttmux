//! The attached view: a terminal, a socket, and nothing else.
//!
//! The client owns no panes. It ships local events to the server and paints
//! what comes back, so killing it -- or losing the ssh session under it --
//! costs nothing but the view.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event;
use crossterm::terminal;
use crossterm::{execute, queue};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Cell;

use crate::config::Config;
use crate::proto::{self, ClientMsg, ServerMsg};

/// Long enough for a just-forked server to have bound its socket, short
/// enough that a genuinely dead session is not a hang. Bounded, not a spin.
const CONNECT_TRIES: u32 = 50;
const CONNECT_WAIT: Duration = Duration::from_millis(20);

// ------------------------------------------------------- terminal guard

/// Saved before raw mode so a signal handler can put the terminal back
/// without calling anything that takes a lock.
static ORIG_TERMIOS: OnceLock<libc::termios> = OnceLock::new();

/// Bracketed paste off, mouse reporting off, alternate screen off, cursor on
/// -- the same modes `restore` turns off, spelled out so the signal handler
/// can emit them with a bare `write(2)`.
const RESET: &[u8] =
    b"\x1b[?2004l\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1049l\x1b[?25h";

/// A client that exits leaving raw mode on and the alternate screen up hands
/// the user a dead shell, so the teardown hangs off `Drop` and runs on every
/// path out: normal return, `?`, and unwinding panic alike.
struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

fn setup(cfg: &Config) -> Result<()> {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) == 0 {
            let _ = ORIG_TERMIOS.set(t);
        }
        // Through a fn pointer, not straight from the fn item: casting the
        // item to an integer is its own clippy lint.
        let handler: extern "C" fn(i32) = on_signal;
        for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
            libc::signal(sig, handler as libc::sighandler_t);
        }
    }
    terminal::enable_raw_mode()?;
    let mut out = io::stdout();
    let entered = (|| -> io::Result<()> {
        execute!(out, terminal::EnterAlternateScreen)?;
        if cfg.general.mouse {
            execute!(out, event::EnableMouseCapture)?;
        }
        execute!(out, event::EnableBracketedPaste)
    })();
    if entered.is_err() {
        restore();
    }
    Ok(entered?)
}

fn restore() {
    let mut out = io::stdout();
    let _ = execute!(
        out,
        event::DisableBracketedPaste,
        event::DisableMouseCapture,
        terminal::LeaveAlternateScreen,
        Show
    );
    let _ = terminal::disable_raw_mode();
    let _ = out.flush();
}

/// Async-signal-safe teardown: `tcsetattr` and `write` only, then `_exit`.
/// A SIGTERM or a closed ssh session must not leave the terminal in raw mode.
extern "C" fn on_signal(sig: i32) {
    unsafe {
        if let Some(t) = ORIG_TERMIOS.get() {
            libc::tcsetattr(0, libc::TCSANOW, t);
        }
        libc::write(1, RESET.as_ptr().cast(), RESET.len());
        libc::_exit(128 + sig);
    }
}

// -------------------------------------------------------------- attaching

fn connect(path: &Path, tries: u32) -> Result<UnixStream> {
    let mut last = None;
    for _ in 0..tries {
        match UnixStream::connect(path) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
        thread::sleep(CONNECT_WAIT);
    }
    Err(last.unwrap_or_else(|| io::Error::other("no attempt"))).context("connect")
}

/// Attach to `session`, starting a server for it first if `create` is set.
pub fn attach(session: &str, create: bool) -> Result<()> {
    let path = proto::socket_path(session)?;
    let mut sock = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(_) if create => {
            crate::server::spawn(session)?;
            connect(&path, CONNECT_TRIES)?
        }
        Err(e) => {
            return Err(e).with_context(|| format!("no server for session {session:?}"));
        }
    };

    let cfg = Config::load(&crate::config::config_path()).unwrap_or_default();
    let (cols, rows) = terminal::size().context("terminal size")?;
    setup(&cfg)?;
    let _guard = Guard;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        hook(info);
    }));

    proto::write_msg(
        &mut sock,
        &ClientMsg::Hello {
            proto: proto::PROTOCOL,
            cols,
            rows,
            term: std::env::var("TERM").unwrap_or_default(),
        },
    )?;

    // Local events go up on their own thread; this one only paints. The
    // thread dies with the process, which is why it is never joined.
    let mut up = sock.try_clone()?;
    thread::spawn(move || loop {
        match event::read() {
            Ok(ev) => {
                if proto::write_msg(&mut up, &ClientMsg::Input(ev)).is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    });

    paint_loop(&mut sock)
}

fn paint_loop(sock: &mut UnixStream) -> Result<()> {
    let mut back = CrosstermBackend::new(io::stdout());
    let mut greeted = false;
    loop {
        let Some(msg) = proto::read_msg::<_, ServerMsg>(sock)? else {
            return Ok(());
        };
        match msg {
            ServerMsg::Welcome { proto: v, .. } => {
                if v != proto::PROTOCOL {
                    bail!(
                        "server speaks protocol {v}, this client speaks {}",
                        proto::PROTOCOL
                    );
                }
                greeted = true;
            }
            _ if !greeted => bail!("server did not say hello"),
            ServerMsg::Draw(cells) => {
                let cells: Vec<(u16, u16, Cell)> = cells
                    .into_iter()
                    .map(|c| {
                        let mut cell = Cell::EMPTY;
                        cell.set_symbol(&c.symbol).set_style(c.style);
                        (c.x, c.y, cell)
                    })
                    .collect();
                back.draw(cells.iter().map(|(x, y, c)| (*x, *y, c)))?;
                Backend::flush(&mut back)?;
            }
            ServerMsg::Clear => back.clear()?,
            ServerMsg::Cursor(pos) => {
                let mut out = io::stdout();
                match pos {
                    Some((x, y)) => queue!(out, MoveTo(x, y), Show)?,
                    None => queue!(out, Hide)?,
                }
                out.flush()?;
            }
            // Verbatim, after the frame and at the cursor the server left:
            // inline images are placed by the terminal, not by the cell grid.
            ServerMsg::Passthrough(bytes) => {
                let mut out = io::stdout();
                out.write_all(&bytes)?;
                out.flush()?;
            }
            ServerMsg::Bye(_) => return Ok(()),
            ServerMsg::Error(e) => bail!("{e}"),
        }
    }
}

/// Ask the server for `session` to shut down, and wait until it has.
pub fn kill(session: &str) -> Result<()> {
    let path = proto::socket_path(session)?;
    let mut sock =
        UnixStream::connect(&path).with_context(|| format!("no server for session {session:?}"))?;
    proto::write_msg(&mut sock, &ClientMsg::KillServer)?;
    // The server closes the socket on its way out, so EOF means "gone" and
    // `kill-session` is honest about having finished.
    let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
    while let Ok(Some(_)) = proto::read_msg::<_, ServerMsg>(&mut sock) {}
    Ok(())
}
