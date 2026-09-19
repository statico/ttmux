//! The session server: an `App` running with no terminal of its own.
//!
//! The server owns the ptys and the `App`; clients are only viewers. That is
//! what makes detaching free: a client can come and go, or be replaced by a
//! newer build, without the session noticing. `ttmux upgrade` goes further
//! and moves the session itself into a new server -- see [`crate::migrate`].

use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossterm::event::Event;
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::{Buffer, Cell, CellDiffOption};
use ratatui::layout::{Position, Size};
use ratatui::style::Style;
use ratatui::Terminal;

use crate::app::{App, Exit, Handover, Host, ScriptJob};
use crate::config::Config;
use crate::layout::{PaneId, Rect};
use crate::migrate;
use crate::proto::{self, ClientMsg, ServerMsg, WireCell};
use crate::script;

/// What the session is sized to before anyone attaches, and what it keeps
/// when the last client leaves: a detached session has to be *some* size.
const DEFAULT_SIZE: (u16, u16) = (80, 24);

/// Frames a client may fall behind before it is dropped. The app thread must
/// never block on a socket -- one peer that stopped reading would freeze the
/// session for everyone -- so every client gets a queue and its own writer.
const QUEUE: usize = 256;

/// Only the writer threads ever block on a socket, and this bounds how long
/// one of them lingers on a peer that has stopped reading entirely.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// A client that connects and says nothing keeps a thread; the handshake is
/// one small frame, so anything slower than this is not a real client.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

// ------------------------------------------------------------------- hub

struct Client {
    id: u64,
    tx: SyncSender<ServerMsg>,
    cols: u16,
    rows: u16,
}

struct State {
    clients: Vec<Client>,
    size: (u16, u16),
    /// The last painted frame, so a client attaching mid-session gets a full
    /// repaint without asking the app to redraw.
    screen: Buffer,
    cursor: Option<(u16, u16)>,
    next_id: u64,
    /// Held so shutdown can wait for the last frame to reach the wire. A
    /// writer killed mid-frame hands its client a truncated one, which the
    /// client reports as an error rather than as the session ending.
    writers: Vec<thread::JoinHandle<()>>,
}

/// Everything the client threads, the backend and the host share.
struct Hub {
    state: Mutex<State>,
    events: Sender<Event>,
    /// Scripted commands on their way to the app thread, which picks them up
    /// on its next pass round the loop.
    jobs: Sender<ScriptJob>,
    kill: AtomicBool,
    /// Colours the last client to say hello reported, for the app to take.
    colours: Mutex<Option<(String, String)>>,
    /// Environment the last client to say hello brought, for the app to take.
    env: Mutex<Option<Vec<(String, String)>>>,
    /// `ttmux upgrade` requests waiting on the app thread to pack up.
    handovers: Mutex<Vec<Handover>>,
}

/// Set while the panes are being handed to a new server. Then nothing here
/// may kill them -- not shutdown, not `kill-server`, not a panic. A static,
/// because the panic hook has no `Hub`.
static HANDING_OVER: AtomicBool = AtomicBool::new(false);

impl Hub {
    fn broadcast(&self, msg: &ServerMsg) {
        let mut st = self.state.lock().unwrap();
        self.broadcast_locked(&mut st, msg);
    }

    /// Never blocks: a client whose queue is full is one that has stopped
    /// reading, and it is dropped rather than allowed to stall the app.
    fn broadcast_locked(&self, st: &mut State, msg: &ServerMsg) {
        let before = st.clients.len();
        st.clients.retain(|c| c.tx.try_send(msg.clone()).is_ok());
        if st.clients.len() != before {
            self.resize_locked(st);
        }
    }

    /// tmux's rule: the session is as big as its smallest viewer, so every
    /// attached client sees a whole frame instead of a clipped one. With
    /// nobody attached the geometry is simply kept.
    fn resize_locked(&self, st: &mut State) {
        let Some(size) = st
            .clients
            .iter()
            .map(|c| (c.cols, c.rows))
            .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1)))
        else {
            return;
        };
        if size != st.size {
            st.size = size;
            // The app only learns about geometry through its event stream.
            let _ = self.events.send(Event::Resize(size.0, size.1));
        }
    }

    fn drop_client(&self, id: u64) {
        let mut st = self.state.lock().unwrap();
        st.clients.retain(|c| c.id != id);
        self.resize_locked(&mut st);
    }

    fn detach_all(&self, reason: &str) {
        let mut st = self.state.lock().unwrap();
        let bye = ServerMsg::Bye(reason.to_string());
        for c in st.clients.iter() {
            let _ = c.tx.try_send(bye.clone());
        }
        // Dropping the senders ends the writer threads once they have drained.
        st.clients.clear();
    }
}

// --------------------------------------------------------------- backend

/// Turns what ratatui already diffed into wire frames. There is no frame
/// format of our own here: ratatui computes the diff, this only ships it.
struct WireBackend {
    hub: Arc<Hub>,
    cursor: Position,
}

fn cell_style(cell: &Cell) -> Style {
    Style::new()
        .fg(cell.fg)
        .bg(cell.bg)
        .underline_color(cell.underline_color)
        .add_modifier(cell.modifier)
}

impl Backend for WireBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut st = self.hub.state.lock().unwrap();
        let mut cells = Vec::new();
        for (x, y, cell) in content {
            if let Some(slot) = st.screen.cell_mut((x, y)) {
                *slot = cell.clone();
            }
            // Skipped cells belong to whatever drew over them (wide glyphs,
            // images); a backend must leave them alone.
            if cell.diff_option != CellDiffOption::Skip {
                cells.push(WireCell {
                    x,
                    y,
                    symbol: cell.symbol().to_string(),
                    style: cell_style(cell),
                });
            }
        }
        if !cells.is_empty() {
            self.hub.broadcast_locked(&mut st, &ServerMsg::Draw(cells));
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        let mut st = self.hub.state.lock().unwrap();
        st.cursor = None;
        self.hub.broadcast_locked(&mut st, &ServerMsg::Cursor(None));
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        // `set_cursor_position` always follows, and that is what is sent.
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.cursor = position.into();
        let pos = (self.cursor.x, self.cursor.y);
        let mut st = self.hub.state.lock().unwrap();
        st.cursor = Some(pos);
        self.hub
            .broadcast_locked(&mut st, &ServerMsg::Cursor(Some(pos)));
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        self.clear_region(ClearType::All)
    }

    fn clear_region(&mut self, _clear_type: ClearType) -> io::Result<()> {
        let mut st = self.hub.state.lock().unwrap();
        let (w, h) = st.size;
        st.screen = Buffer::empty(ratatui::layout::Rect::new(0, 0, w, h));
        self.hub.broadcast_locked(&mut st, &ServerMsg::Clear);
        Ok(())
    }

    fn size(&self) -> io::Result<Size> {
        let (w, h) = self.hub.state.lock().unwrap().size;
        Ok(Size::new(w, h))
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size()?,
            // Nobody here knows the client's cell size, and no caller needs it.
            pixels: Size::new(0, 0),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ------------------------------------------------------------------ host

struct ServerHost {
    hub: Arc<Hub>,
    rx: Receiver<Event>,
    jobs: Receiver<ScriptJob>,
}

impl Host for ServerHost {
    fn poll(&mut self, timeout: Duration) -> Result<Option<Event>> {
        // An error here ends `main_loop`, so it is reserved for the one case
        // that should: `kill-server`. Clients coming and going are not it.
        if self.hub.kill.load(Ordering::SeqCst) {
            bail!("kill-server");
        }
        match self.rx.recv_timeout(timeout) {
            Ok(e) => Ok(Some(e)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => bail!("event channel closed"),
        }
    }

    fn passthrough(&mut self, bytes: &[u8]) -> Result<()> {
        self.hub.broadcast(&ServerMsg::Passthrough(bytes.to_vec()));
        Ok(())
    }

    fn commands(&mut self) -> Vec<ScriptJob> {
        self.jobs.try_iter().collect()
    }

    fn colours(&mut self) -> Option<(String, String)> {
        self.hub.colours.lock().unwrap().take()
    }

    fn env(&mut self) -> Option<Vec<(String, String)>> {
        self.hub.env.lock().unwrap().take()
    }

    fn handovers(&mut self) -> Vec<Handover> {
        std::mem::take(&mut *self.hub.handovers.lock().unwrap())
    }
}

// --------------------------------------------------------------- clients

fn accept_loop(listener: UnixListener, hub: Arc<Hub>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { break };
        let hub = hub.clone();
        thread::spawn(move || client_thread(stream, hub));
    }
}

fn client_thread(stream: UnixStream, hub: Arc<Hub>) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = stream.set_read_timeout(Some(HELLO_TIMEOUT));
    let mut rd = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut out = stream;

    let (cols, rows, _term) = match proto::read_msg::<_, ClientMsg>(&mut rd) {
        Ok(Some(ClientMsg::Hello {
            proto: v,
            cols,
            rows,
            term,
            colours,
            env,
        })) => {
            if colours.is_some() {
                *hub.colours.lock().unwrap() = colours;
            }
            if v != proto::PROTOCOL {
                let why = format!(
                    "client speaks protocol {v}, this server speaks {}",
                    proto::PROTOCOL
                );
                let _ = proto::write_msg(&mut out, &ServerMsg::Error(why.clone()));
                let _ = proto::write_msg(&mut out, &ServerMsg::Bye(why));
                return;
            }
            *hub.env.lock().unwrap() = Some(env);
            (cols.max(1), rows.max(1), term)
        }
        // `kill-server` is a one-shot command, not a view: it does not need
        // a terminal and so never says hello.
        Ok(Some(ClientMsg::KillServer)) => {
            hub.kill.store(true, Ordering::SeqCst);
            park();
        }
        // Also one-shot: a script hands over a command, reads the answer and
        // leaves. It never becomes a viewer, so it never resizes the session.
        Ok(Some(ClientMsg::Command { argv, pane })) => {
            let (code, text) = run_command(&hub, argv, pane);
            let _ = proto::write_msg(&mut out, &ServerMsg::Reply { code, text });
            return;
        }
        // Frozen: the one question any version may ask any other.
        Ok(Some(ClientMsg::Version)) => {
            let _ = proto::write_msg(
                &mut out,
                &ServerMsg::Welcome {
                    proto: proto::PROTOCOL,
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    pid: std::process::id(),
                },
            );
            return;
        }
        // A newly forked server asking for the session. If it gets it, this
        // process is finished -- see `hand_over`.
        Ok(Some(ClientMsg::Adopt)) => {
            hand_over(&hub, &mut out);
            return;
        }
        _ => return,
    };
    let _ = rd.set_read_timeout(None);
    // A client that stops reading must not wedge the writer: the queue fills,
    // the socket buffer fills, and then shutdown waits on a thread that is
    // blocked forever. A client this far behind is gone.
    let _ = out.set_write_timeout(Some(Duration::from_secs(2)));

    let (tx, rx) = mpsc::sync_channel(QUEUE);
    let id = {
        let mut st = hub.state.lock().unwrap();
        let id = st.next_id;
        st.next_id += 1;
        // Queued under the lock, so anything the app paints from here on
        // lands behind the snapshot rather than under it.
        for msg in [
            ServerMsg::Welcome {
                proto: proto::PROTOCOL,
                version: env!("CARGO_PKG_VERSION").to_string(),
                pid: std::process::id(),
            },
            ServerMsg::Clear,
            ServerMsg::Draw(snapshot(&st.screen)),
            ServerMsg::Cursor(st.cursor),
        ] {
            let _ = tx.try_send(msg);
        }
        st.clients.push(Client { id, tx, cols, rows });
        hub.resize_locked(&mut st);
        id
    };
    {
        let inner = hub.clone();
        let writer = thread::spawn(move || {
            for msg in rx {
                match proto::write_msg(&mut out, &msg) {
                    // Too big to send is checked before anything is written:
                    // lose the message, not the client.
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {}
                    Err(_) => break,
                    Ok(()) => {}
                }
            }
            inner.drop_client(id);
        });
        hub.state.lock().unwrap().writers.push(writer);
    }

    loop {
        match proto::read_msg::<_, ClientMsg>(&mut rd) {
            Ok(Some(ClientMsg::Input(Event::Resize(w, h)))) => {
                // A client's resize is that client's size, not the session's;
                // the session follows the minimum, which `resize_locked` sends
                // on to the app as its own `Event::Resize`.
                let mut st = hub.state.lock().unwrap();
                if let Some(c) = st.clients.iter_mut().find(|c| c.id == id) {
                    c.cols = w.max(1);
                    c.rows = h.max(1);
                }
                hub.resize_locked(&mut st);
            }
            Ok(Some(ClientMsg::Input(ev))) => {
                let _ = hub.events.send(ev);
            }
            Ok(Some(ClientMsg::Detach)) => {
                // Through the client entry rather than a sender of our own:
                // a clone held here outlives `park()` below, and shutdown
                // waits on the writer that the last sender keeps alive.
                let st = hub.state.lock().unwrap();
                if let Some(c) = st.clients.iter().find(|c| c.id == id) {
                    let _ = c.tx.try_send(ServerMsg::Bye("detached".into()));
                }
                drop(st);
                break;
            }
            Ok(Some(ClientMsg::Hello { .. }))
            | Ok(Some(ClientMsg::Version))
            | Ok(Some(ClientMsg::Adopt))
            | Ok(Some(ClientMsg::Adopted)) => {}
            Ok(Some(ClientMsg::KillServer)) => {
                hub.kill.store(true, Ordering::SeqCst);
                park();
            }
            Ok(Some(ClientMsg::Command { argv, pane })) => {
                let (code, text) = run_command(&hub, argv, pane);
                let st = hub.state.lock().unwrap();
                if let Some(c) = st.clients.iter().find(|c| c.id == id) {
                    let _ = c.tx.try_send(ServerMsg::Reply { code, text });
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    hub.drop_client(id);
}

/// Parse one scripted command and run it on the app thread, waiting for the
/// answer. Parse errors never reach the app: they are the script's mistake.
fn run_command(hub: &Hub, argv: Vec<String>, caller: Option<PaneId>) -> (u8, String) {
    let Some((verb, args)) = argv.split_first() else {
        return (script::EXIT_USAGE, "no command".into());
    };
    let cmd = match script::parse(verb, args) {
        Ok(c) => c,
        Err(e) => return (script::EXIT_USAGE, format!("{e:#}")),
    };
    let (tx, rx) = mpsc::sync_channel(1);
    if hub
        .jobs
        .send(ScriptJob {
            cmd,
            caller,
            reply: tx,
        })
        .is_err()
    {
        return (script::EXIT_ERROR, "session is shutting down".into());
    }
    // The app answers within a tick; a longer wait means it is wedged, and a
    // script that hangs forever is worse than one that reports it.
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(text)) => (script::EXIT_OK, text),
        Ok(Err(e)) => (script::EXIT_ERROR, e),
        Err(_) => (script::EXIT_ERROR, "session did not answer".into()),
    }
}

/// Hold a connection open until the process exits, so that the peer's EOF
/// means "the session is gone" and `kill-session` can wait for it honestly.
fn park() -> ! {
    loop {
        thread::sleep(Duration::from_millis(50));
    }
}

/// Every non-blank cell of the last frame. The client has just been told to
/// clear, so blanks are already correct and not worth the bytes.
fn snapshot(screen: &Buffer) -> Vec<WireCell> {
    let area = screen.area;
    let mut out = Vec::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let Some(cell) = screen.cell((x, y)) else {
                continue;
            };
            if cell.diff_option == CellDiffOption::Skip || cell == &Cell::EMPTY {
                continue;
            }
            out.push(WireCell {
                x,
                y,
                symbol: cell.symbol().to_string(),
                style: cell_style(cell),
            });
        }
    }
    out
}

/// Give the session away and stop being it.
///
/// The app thread does the packing, because it is the only thread that may
/// touch the panes. If anything at all goes wrong the answer is to say so and
/// carry on serving: the session is still here and still whole.
fn hand_over(hub: &Hub, out: &mut UnixStream) {
    // Two at once would duplicate every pty into two servers, both of them
    // reading it and both of them armed to kill the shell.
    if HANDING_OVER.swap(true, Ordering::SeqCst) {
        let _ = proto::write_msg(
            out,
            &ServerMsg::Error("a handover is already running".into()),
        );
        return;
    }
    let (tx, rx) = mpsc::sync_channel(1);
    hub.handovers.lock().unwrap().push(Handover { reply: tx });
    let packed = rx
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|_| Err("session did not respond".into()));
    let (snap, fds) = match packed {
        Ok(packed) => packed,
        Err(e) => {
            HANDING_OVER.store(false, Ordering::SeqCst);
            let _ = proto::write_msg(out, &ServerMsg::Error(e));
            return;
        }
    };
    // ponytail: if this side's wait for the confirmation times out (120s:
    // a machine asleep, a SIGSTOP) after the new server has renamed over the
    // socket, this server keeps shells nobody can reach. Closing that means
    // a two-phase commit between the two servers.
    if let Err(e) = migrate::give(out, &snap, &fds) {
        // Nothing more goes down this socket: a failed `give` may have left
        // half a frame on it, and another frame behind that is just noise the
        // far end would try to parse as a session.
        eprintln!("handover failed, keeping the session: {e:#}");
        HANDING_OVER.store(false, Ordering::SeqCst);
        return;
    }
    eprintln!("handed {} panes over; leaving them running", fds.len());
    // Best effort: waiting for the writers to send this would also keep the
    // reader threads here eating pane output the new server never sees.
    hub.detach_all("upgraded -- reattach with `ttmux attach`");
    // Every pane belongs to the new server now. Returning would drop the app,
    // which kills them, so nothing here may run a destructor ever again.
    unsafe { libc::_exit(0) }
}

// ------------------------------------------------------------ daemonising

/// Start a server for `session` and return once its socket will accept
/// connections. Does nothing if one is already running.
pub fn spawn(session: &str) -> Result<()> {
    spawn_at(session, None)
}

/// Start a server for `session` that takes the session over from the one
/// already running at `from`, and return once it has.
///
/// Forked from the caller, so it inherits whatever identity the caller has --
/// which is the entire point of [`crate::migrate`].
pub fn spawn_adopting(session: &str, from: &Path) -> Result<()> {
    spawn_at(session, Some(from))
}

fn spawn_at(session: &str, adopt: Option<&Path>) -> Result<()> {
    let path = proto::socket_path(session)?;
    if adopt.is_none() && proto::is_live(&path) {
        return Ok(());
    }
    // While adopting, the real path is still the running server's: bind
    // beside it and move over it once the session is safely here.
    let bind = match adopt {
        Some(_) => migrate::pending_path(&path),
        None => path.clone(),
    };
    // A crashed server leaves the file behind; bind would fail on it.
    let _ = fs::remove_file(&bind);
    // Bound in the *parent*, before the fork: the caller attaches the moment
    // this returns, and the kernel queues connections on a listening socket
    // whether or not the child has reached `accept` yet. Binding in the child
    // would mean racing it, and a bind error would have nowhere to be printed.
    let listener = UnixListener::bind(&bind).with_context(|| format!("bind {}", bind.display()))?;
    let _ = fs::set_permissions(&bind, fs::Permissions::from_mode(0o600));
    // The app reads the session name from the environment. `TTMUX` is how a
    // pane knows it is inside this session, as `$TMUX` is for tmux.
    std::env::set_var("TTMUX_SESSION", session);
    std::env::set_var("TTMUX", &path);

    // fork, setsid, fork again: the first fork leaves the shell's job control,
    // setsid drops the controlling terminal (or the shell's SIGHUP on exit
    // takes the session with it), and the second fork makes the daemon a
    // non-session-leader so it can never acquire a controlling terminal again.
    match unsafe { libc::fork() } {
        -1 => bail!("fork: {}", io::Error::last_os_error()),
        0 => {}
        pid => {
            drop(listener);
            // The intermediate child exits at once; reap it so the caller is
            // not left with a zombie for the rest of its life.
            let mut status = 0;
            unsafe { libc::waitpid(pid, &mut status, 0) };
            return Ok(());
        }
    }
    unsafe { libc::setsid() };
    match unsafe { libc::fork() } {
        0 => {}
        _ => unsafe { libc::_exit(0) },
    }
    redirect_stdio(&log_path(session));
    no_unwinding();
    let adopt = adopt.map(|from| Adoption {
        from: from.to_path_buf(),
        pending: bind,
        path: path.clone(),
    });
    let code = i32::from(serve(listener, &path, adopt).is_err());
    unsafe { libc::_exit(code) }
}

/// What a forked server needs to take a session over: who has it, where this
/// server's socket is now, and where it belongs once the session is here.
struct Adoption {
    from: PathBuf,
    pending: PathBuf,
    path: PathBuf,
}

/// During a handover, turn a panic into an immediate exit.
///
/// Unwinding out of the app thread drops the `App`, and dropping an `App`
/// signals every pane's process group -- shells that now belong to another
/// server. The hook runs *before* the unwind, so no destructor runs. At any
/// other time a panic unwinds as it always did.
fn no_unwinding() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        if HANDING_OVER.load(Ordering::SeqCst) {
            unsafe { libc::_exit(101) }
        }
    }));
}

/// Where the daemon's stderr goes. `$TTMUX_LOG` overrides it.
pub fn log_path(session: &str) -> PathBuf {
    if let Some(p) = std::env::var_os("TTMUX_LOG") {
        return PathBuf::from(p);
    }
    proto::socket_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(format!("{session}.log"))
}

/// stdio still points at the user's terminal until this runs, and the daemon
/// writing to it would scribble over the client's screen.
///
/// stderr goes to a file rather than to /dev/null: a panic in a detached
/// session is otherwise completely silent, which leaves a crash with no
/// evidence at all.
fn redirect_stdio(log: &Path) {
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            libc::dup2(null, 0);
            libc::dup2(null, 1);
            if null > 2 {
                libc::close(null);
            }
        }
        let Ok(c) = std::ffi::CString::new(log.as_os_str().as_bytes()) else {
            return;
        };
        let fd = libc::open(
            c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
            0o600,
        );
        if fd < 0 {
            if null >= 0 {
                libc::dup2(null, 2);
            }
            return;
        }
        libc::dup2(fd, 2);
        if fd > 2 {
            libc::close(fd);
        }
    }
    eprintln!(
        "--- ttmux {} started, pid {}",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    );
}

/// Run the session until it quits. Consumes the already-bound listener.
fn serve(listener: UnixListener, path: &Path, adopt: Option<Adoption>) -> Result<()> {
    let pending = adopt.as_ref().map(|a| a.pending.clone());
    let result = run(listener, adopt);
    // Whichever socket this process is on. Until the rename it is still the
    // pending one, and `path` belongs to the server that still has the
    // session; after it, the pending name is gone and `path` is ours.
    // An old server stopped mid-handover may already have been renamed
    // over: `path` is then the new server's.
    if !HANDING_OVER.load(Ordering::SeqCst) {
        let _ = fs::remove_file(match pending {
            Some(pending) if pending.exists() => pending,
            _ => path.to_path_buf(),
        });
    }
    result
}

fn run(listener: UnixListener, adopt: Option<Adoption>) -> Result<()> {
    let cfg_path = crate::config::config_path();
    let cfg = Config::load(&cfg_path).unwrap_or_default();
    // Everything the other server has to say happens here, before a single
    // pane is replayed: it is holding the session open until this returns,
    // and both processes are reading the same ptys until it lets go.
    let taken = match &adopt {
        None => None,
        Some(a) => {
            let (snap, fds, mut sock) = migrate::take(&a.from)?;
            // Take the name over before saying so: the old server exits on
            // the word, and a client attaching in between must find a server
            // here. The listener is already bound, so connections queue.
            fs::rename(&a.pending, &a.path)
                .with_context(|| format!("take over {}", a.path.display()))?;
            // The old server exits on this.
            proto::write_msg(&mut sock, &ClientMsg::Adopted)?;
            Some((snap, fds))
        }
    };
    // An adopted session keeps the size it had: laying it out at 80x24 first
    // would truncate every pane's replay before a client ever attached.
    let (w, h) = match &taken {
        Some((snap, _)) if snap.cols > 0 && snap.rows > 0 => (snap.cols, snap.rows),
        _ => DEFAULT_SIZE,
    };
    let (tx, rx) = mpsc::channel();
    let (jobs_tx, jobs_rx) = mpsc::channel();
    let hub = Arc::new(Hub {
        state: Mutex::new(State {
            clients: vec![],
            size: (w, h),
            screen: Buffer::empty(ratatui::layout::Rect::new(0, 0, w, h)),
            cursor: None,
            next_id: 1,
            writers: vec![],
        }),
        jobs: jobs_tx,
        events: tx,
        kill: AtomicBool::new(false),
        colours: Mutex::new(None),
        env: Mutex::new(None),
        handovers: Mutex::new(vec![]),
    });

    let area = Rect::new(0, 0, w, h);
    let mut app = match taken {
        None => App::new(cfg, cfg_path, area)?,
        Some((snap, fds)) => App::adopt(cfg, cfg_path, area, snap, fds),
    };
    let mut term = Terminal::new(WireBackend {
        hub: hub.clone(),
        cursor: Position::new(0, 0),
    })?;
    let mut host = ServerHost {
        hub: hub.clone(),
        rx,
        jobs: jobs_rx,
    };
    {
        let hub = hub.clone();
        thread::spawn(move || accept_loop(listener, hub));
    }

    let result = (|| -> Result<()> {
        loop {
            let exit = app.main_loop(&mut term, &mut host)?;
            if exit == Exit::Quit || hub.kill.load(Ordering::SeqCst) {
                return Ok(());
            }
            // The app detached: there is one shared view, so that means every
            // client leaves. The session itself keeps running, which is the
            // whole point -- a per-client `Detach` never gets this far.
            hub.detach_all("detached");
            app.detached = false;
        }
    })();

    // Dropping the app kills every pane and its process group -- which is
    // right for a session that is ending, and fatal for one that has just
    // been handed to another server.
    if HANDING_OVER.load(Ordering::SeqCst) {
        std::mem::forget(app);
    } else {
        drop(app);
    }
    hub.detach_all("server exiting");
    // `detach_all` dropped every sender, so each writer ends once it has
    // drained. Joining outside the lock, because the writers take it on the
    // way out.
    let writers = std::mem::take(&mut hub.state.lock().unwrap().writers);
    for w in writers {
        let _ = w.join();
    }
    result
}
