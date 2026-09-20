//! One pseudo-terminal per pane: child process, terminal emulator, scrollback.
//!
//! Output is read on a background thread into a channel; [`Pane::pump`] drains
//! that channel into the emulator and never blocks.

use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use portable_pty::{Child, ChildKiller, CommandBuilder, ExitStatus, MasterPty, PtySize};

use crate::config::Config;
use crate::graphics::{self, Image, Piece, Scanner};
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
    /// Answers to the child's terminal queries, written back after each pump.
    replies: Vec<u8>,
    /// OSC 52 copies meant for the outer terminal.
    copies: Vec<u8>,
    /// Desktop notifications meant for the outer terminal.
    notices: Vec<u8>,
    /// DECSET 1004: the child wants `CSI I` and `CSI O` on focus changes.
    focus_events: bool,
    /// DECSCUSR shape, 0 being the terminal's default.
    cursor_shape: u16,
    /// DECSET 2026: when the child began a synchronized update.
    synced: Option<Instant>,
    /// The last OSC 52 copy, base64, for a child that asks to paste.
    clipboard: Vec<u8>,
    /// OSC 10 and 11 answers, `rgb:rrrr/gggg/bbbb`, when the outer terminal
    /// told us its colours.
    pub colours: Option<(String, String)>,
    /// Kitty keyboard flag stacks, main screen then alternate, as the spec
    /// keeps one per screen.
    kitty: [Vec<u8>; 2],
    /// xterm modifyOtherKeys level.
    modify_other_keys: u8,
    /// DECSET 2031: the child wants `CSI ? 997 ; n n` when the theme flips.
    theme_reports: bool,
}

/// The kitty keyboard flags ttmux honours: disambiguate (1) and report all
/// keys as escapes (8). Event types, alternates and text need the outer
/// terminal to report them, and a query answers with what is really on.
// ponytail: add 2, 4 and 16 when something needs key releases.
const KITTY_FLAGS: u8 = 1 | 8;

impl Sink {
    fn kitty_stack(&mut self, screen: &vt100::Screen) -> &mut Vec<u8> {
        &mut self.kitty[usize::from(screen.alternate_screen())]
    }
}

/// `CSI ? 997 ; n n`'s n for a background `rgb:rrrr/gggg/bbbb`: 1 dark, 2 light.
fn theme_of(colours: &Option<(String, String)>) -> Option<u8> {
    let bg = colours.as_ref()?.1.strip_prefix("rgb:")?;
    let mut parts = bg
        .split('/')
        .map(|h| u8::from_str_radix(h.get(..2)?, 16).ok());
    let (r, g, b) = (parts.next()??, parts.next()??, parts.next()??);
    let luma = 299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b);
    Some(if luma < 128_000 { 1 } else { 2 })
}

/// The terminfo capabilities XTGETTCAP answers, `""` for a boolean.
fn termcap(name: &str) -> Option<&'static str> {
    Some(match name {
        "TN" => "ttmux",
        "Co" | "colors" => "256",
        "RGB" => "8/8/8",
        "Tc" => "",
        "Smulx" => "\\E[4:%p1%dm",
        "Setulc" => "\\E[58:2::%p1%{65536}%/%d:%p1%{256}%/%{255}%&%d:%p1%{255}%&%d%;m",
        "Ss" => "\\E[%p1%d q",
        "Se" => "\\E[2 q",
        "Ms" => "\\E]52;%p1%s;%p2%s\\007",
        "Sync" => "\\E[?2026%?%p1%{1}%-%tl%eh%;",
        _ => return None,
    })
}

fn unhex(hex: &[u8]) -> Option<String> {
    let bytes = hex
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).ok()?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    String::from_utf8(bytes).ok()
}

fn to_hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02X}")).collect()
}

fn as_str(b: &[u8]) -> &str {
    std::str::from_utf8(b).unwrap_or_default()
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            out.push(if i <= c.len() {
                ABC[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

/// Longest a synchronized update may hold the pane's frame, as tmux does,
/// so a child that dies mid-update does not freeze its pane.
const SYNC_TIMEOUT: Duration = Duration::from_secs(1);

/// DECRPM's answer for a DEC private mode: 1 set, 2 reset, 0 unknown.
fn mode_status(sink: &Sink, screen: &vt100::Screen, mode: u16) -> u8 {
    use vt100::{MouseProtocolEncoding as E, MouseProtocolMode as M};
    let on = match mode {
        1 => screen.application_cursor(),
        25 => !screen.hide_cursor(),
        47 | 1047 | 1049 => screen.alternate_screen(),
        9 => screen.mouse_protocol_mode() == M::Press,
        1000 => screen.mouse_protocol_mode() == M::PressRelease,
        1002 => screen.mouse_protocol_mode() == M::ButtonMotion,
        1003 => screen.mouse_protocol_mode() == M::AnyMotion,
        1005 => screen.mouse_protocol_encoding() == E::Utf8,
        1006 => screen.mouse_protocol_encoding() == E::Sgr,
        1004 => sink.focus_events,
        2004 => screen.bracketed_paste(),
        2026 => sink.synced.is_some(),
        2031 => sink.theme_reports,
        _ => return 0,
    };
    if on {
        1
    } else {
        2
    }
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
    /// The queries a program blocks on, and the modes vt100 does not keep.
    /// fzf, for one, asks where the cursor is and draws nothing until it
    /// hears back; neovim and fish send a batch ending in DA1 and wait.
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let mut reply = |r: String| self.replies.extend_from_slice(r.as_bytes());
        match (i1, i2, c) {
            (None, None, 'n') if first == 5 => reply("\x1b[0n".into()),
            (None, None, 'n') if first == 6 => {
                let (row, col) = screen.cursor_position();
                reply(format!("\x1b[{};{}R", row + 1, col + 1));
            }
            // VT220 with ANSI colour, what tmux answers too.
            (None, None, 'c') if first == 0 => reply("\x1b[?62;22;52c".into()),
            // DA2: a VT220, firmware 10, no ROM cartridge.
            (Some(b'>'), None, 'c') if first == 0 => reply("\x1b[>1;10;0c".into()),
            // XTVERSION: how neovim and yazi tell which terminal this is.
            (Some(b'>'), None, 'q') if first == 0 => {
                reply(format!("\x1bP>|ttmux {}\x1b\\", env!("CARGO_PKG_VERSION")))
            }
            // The text area in characters.
            (None, None, 't') if first == 18 => {
                let (rows, cols) = screen.size();
                reply(format!("\x1b[8;{rows};{cols}t"));
            }
            (Some(b'?'), Some(b'$'), 'p') => {
                let status = mode_status(self, screen, first);
                self.replies
                    .extend_from_slice(format!("\x1b[?{first};{status}$y").as_bytes());
            }
            // vt100 hands over the whole DECSET/DECRST once per mode it did
            // not know, so each pass sets every mode named, idempotently.
            (Some(b'?'), None, 'h' | 'l') => {
                let on = c == 'h';
                for mode in params.iter().filter_map(|p| p.first()) {
                    match mode {
                        1004 => self.focus_events = on,
                        2026 => self.synced = on.then(Instant::now),
                        2031 => self.theme_reports = on,
                        _ => {}
                    }
                }
            }
            (Some(b' '), None, 'q') => self.cursor_shape = first,
            // Kitty keyboard protocol: query, push, pop and set.
            (Some(b'?'), None, 'u') => {
                let flags = self.kitty_stack(screen).last().copied().unwrap_or(0);
                self.replies
                    .extend_from_slice(format!("\x1b[?{flags}u").as_bytes());
            }
            (Some(b'>'), None, 'u') => {
                let stack = self.kitty_stack(screen);
                // The spec lets a full stack forget its oldest entry.
                if stack.len() == 16 {
                    stack.remove(0);
                }
                stack.push(first as u8 & KITTY_FLAGS);
            }
            (Some(b'<'), None, 'u') => {
                let stack = self.kitty_stack(screen);
                stack.truncate(stack.len().saturating_sub(usize::from(first.max(1))));
            }
            (Some(b'='), None, 'u') => {
                let flags = first as u8 & KITTY_FLAGS;
                let mode = params.get(1).and_then(|p| p.first()).copied().unwrap_or(1);
                let stack = self.kitty_stack(screen);
                if stack.is_empty() {
                    stack.push(0);
                }
                let top = stack.last_mut().unwrap();
                *top = match mode {
                    1 => flags,
                    2 => *top | flags,
                    3 => *top & !flags,
                    _ => *top,
                };
            }
            (Some(b'>'), None, 'm') if first == 4 => {
                self.modify_other_keys = params
                    .get(1)
                    .and_then(|p| p.first())
                    .map_or(0, |n| *n as u8);
            }
            (Some(b'?'), None, 'n') if first == 996 => {
                if let Some(theme) = theme_of(&self.colours) {
                    self.replies
                        .extend_from_slice(format!("\x1b[?997;{theme}n").as_bytes());
                }
            }
            _ => {}
        }
    }

    /// XTGETTCAP: neovim asks for `Smulx` and `Setulc` before it draws
    /// undercurls, and for `Ms` before it trusts OSC 52.
    fn unhandled_dcs(&mut self, _: &mut vt100::Screen, i: &[u8], c: char, data: &[u8]) {
        if (i, c) != (b"+", 'q') {
            return;
        }
        for hex in data.split(|b| *b == b';') {
            let name = unhex(hex).unwrap_or_default();
            let reply = match termcap(&name) {
                Some("") => format!("\x1bP1+r{}\x1b\\", as_str(hex)),
                Some(v) => format!("\x1bP1+r{}={}\x1b\\", as_str(hex), to_hex(v)),
                None => format!("\x1bP0+r{}\x1b\\", as_str(hex)),
            };
            self.replies.extend_from_slice(reply.as_bytes());
        }
    }

    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8], data: &[u8]) {
        self.clipboard = data.to_vec();
        self.copies.extend_from_slice(b"\x1b]52;");
        self.copies.extend_from_slice(ty);
        self.copies.push(b';');
        self.copies.extend_from_slice(data);
        self.copies.extend_from_slice(b"\x1b\\");
    }

    /// Answered from the last copy rather than the real clipboard: reading
    /// the outer one would need its reply routed back, and neovim otherwise
    /// waits on this.
    fn paste_from_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8]) {
        self.replies.extend_from_slice(b"\x1b]52;");
        self.replies.extend_from_slice(ty);
        self.replies.push(b';');
        self.replies.extend_from_slice(&self.clipboard);
        self.replies.extend_from_slice(b"\x1b\\");
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]], bel: bool) {
        let end: &str = if bel { "\x07" } else { "\x1b\\" };
        match params {
            // Colour queries: neovim picks light or dark from the answer.
            [n @ (b"10" | b"11"), b"?"] => {
                if let Some((fg, bg)) = &self.colours {
                    let colour = if *n == b"10" { fg } else { bg };
                    let n = std::str::from_utf8(n).unwrap_or_default();
                    self.replies
                        .extend_from_slice(format!("\x1b]{n};{colour}{end}").as_bytes());
                }
            }
            // Desktop notifications go to the terminal that can show them.
            [b"9" | b"99" | b"777", ..] => {
                self.notices.extend_from_slice(b"\x1b]");
                self.notices.extend_from_slice(&params.join(&b';'));
                self.notices.extend_from_slice(b"\x1b\\");
            }
            _ => {}
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
    /// Copy mode is on this pane: hold the view still even at the live
    /// bottom, so a selection does not slide out from under the pointer.
    pub frozen: bool,
    /// What to answer a graphics query with. The app keeps it in step with
    /// `general.passthrough-images`.
    pub images_supported: bool,
    /// `scrolled_off` as of the last pump, to see how far the text moved.
    seen_scrolled_off: usize,
    /// Sticky bell flag, cleared by the app.
    pub bell: bool,
    pub title_override: Option<String>,

    parser: vt100::Parser<Sink>,
    scanner: Scanner,
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
    /// Spawn the configured shell in a new pty, or `command` through it, as
    /// tmux runs `split-window CMD` with `$SHELL -c`.
    pub fn spawn(
        id: PaneId,
        cfg: &Config,
        cwd: Option<PathBuf>,
        command: Option<&str>,
        cols: u16,
        rows: u16,
        env: &[(String, Option<String>)],
    ) -> anyhow::Result<Pane> {
        let shell = if !cfg.general.shell.is_empty() {
            cfg.general.shell.clone()
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
        };
        let mut cmd = CommandBuilder::new(&shell);
        match command {
            Some(c) => cmd.args(["-c", c]),
            None => cmd.args(&cfg.general.shell_args),
        }
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        cmd.env("TERM", "xterm-256color");
        // Every colour a pane emits reaches the outer terminal as 24-bit, so
        // say so: without it a program downgrades to the 16 ANSI colours and
        // paints, say, a badge as reverse video.
        cmd.env("COLORTERM", "truecolor");
        // The server sets `TTMUX` to its socket and panes inherit it; only
        // `--no-daemon` has no socket to name.
        if std::env::var_os("TTMUX").is_none() {
            cmd.env("TTMUX", "1");
        }
        cmd.env("TTMUX_PANE", id.to_string());
        // Last, so a client's `SSH_AUTH_SOCK` wins over the stale one this
        // server was started with. See `general.update_environment`.
        for (k, v) in env {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            }
        }
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

        let parser = vt100::Parser::new_with_callbacks(rows, cols, scrollback, Sink::default());
        Pane::assemble(id, name, parser, pair.master, child)
    }

    /// A pane around a master and its child, however they were come by: the
    /// reader thread and every field's starting value.
    fn assemble(
        id: PaneId,
        name: String,
        parser: vt100::Parser<Sink>,
        master: Box<dyn MasterPty + Send>,
        child: Box<dyn Child + Send + Sync>,
    ) -> anyhow::Result<Pane> {
        let writer = master.take_writer().context("pty writer")?;
        let mut reader = master.try_clone_reader().context("pty reader")?;
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
        let (rows, cols) = parser.screen().size();
        Ok(Pane {
            id,
            cols,
            rows,
            scroll: 0,
            frozen: false,
            // An adopted parser has had the replay through it already.
            seen_scrolled_off: parser.screen().scrolled_off(),
            bell: false,
            title_override: None,
            parser,
            scanner: Scanner::new(),
            images: RefCell::new(Vec::new()),
            images_supported: true,
            rx,
            master,
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
                            Piece::Plain(bytes) => {
                                self.parser.process(&bytes);
                            }
                            // The cursor is wherever the preceding plain bytes
                            // left it, which is where the image belongs.
                            // Anything but drawing is dropped, not replayed.
                            // Answered here, not passed out: see
                            // `graphics::query_reply`.
                            Piece::Image(bytes) => {
                                if let Some(reply) =
                                    graphics::query_reply(&bytes, self.images_supported)
                                {
                                    self.send(&reply);
                                    continue;
                                }
                                if !graphics::is_safe(&bytes) {
                                    continue;
                                }
                                let (row, col) = self.parser.screen().cursor_position();
                                let mut pending = self.images.borrow_mut();
                                // A video redraws the same cell every frame;
                                // only the newest frame is worth sending.
                                pending.retain(|i| (i.row, i.col) != (row, col));
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
        let replies = std::mem::take(&mut self.parser.callbacks_mut().replies);
        if !replies.is_empty() {
            self.send(&replies);
        }
        let sink = self.parser.callbacks_mut();
        if sink.synced.is_some_and(|t| t.elapsed() >= SYNC_TIMEOUT) {
            sink.synced = None;
        }
        let holding = sink.synced.is_some();
        if std::mem::take(&mut self.parser.callbacks_mut().bell) {
            self.bell = true;
        }
        // Rows that scrolled off carry the text up with them. A view that is
        // not live -- scrolled back, or held by copy mode -- follows its text
        // instead, so what is being read or selected stays where it was.
        let now = self.parser.screen().scrolled_off();
        // Down only on a reset (RIS), which takes the history with it.
        let moved = now.saturating_sub(self.seen_scrolled_off);
        self.seen_scrolled_off = now;
        if self.scroll > 0 || self.frozen {
            self.scroll += moved;
        }
        self.parser.screen_mut().set_scrollback(self.scroll);
        self.scroll = self.parser.screen().scrollback();
        // Mid synchronized update the screen is half drawn; the update's end
        // is the change worth a frame.
        !holding
    }

    /// Bytes for the outer terminal since the last call: OSC 52 copies and
    /// desktop notifications, each only when allowed. The rest is discarded.
    pub fn take_host_bytes(&mut self, clipboard: bool, notifications: bool) -> Vec<u8> {
        let sink = self.parser.callbacks_mut();
        let copies = std::mem::take(&mut sink.copies);
        let notices = std::mem::take(&mut sink.notices);
        let mut out = if clipboard { copies } else { vec![] };
        if notifications {
            out.extend(notices);
        }
        out
    }

    /// The DECSCUSR cursor shape the child asked for, 0 for the default.
    pub fn cursor_shape(&self) -> u16 {
        self.parser.callbacks().cursor_shape
    }

    /// Tell the child it gained or lost focus, if it asked to hear.
    pub fn focus_changed(&mut self, focused: bool) {
        if self.parser.callbacks().focus_events {
            self.send(if focused { b"\x1b[I" } else { b"\x1b[O" });
        }
    }

    /// The outer terminal's foreground and background, for OSC 10 and 11.
    /// A child that set DECSET 2031 hears when that flips it light or dark.
    pub fn set_colours(&mut self, colours: Option<(String, String)>) {
        let sink = self.parser.callbacks_mut();
        let was = theme_of(&sink.colours);
        sink.colours = colours;
        let now = theme_of(&sink.colours);
        match now {
            Some(n) if sink.theme_reports && now != was => {
                self.send(format!("\x1b[?997;{n}n").as_bytes());
            }
            _ => {}
        }
    }

    /// How the child asked keys to be encoded, for the screen it is on.
    pub fn keyboard(&self) -> crate::input::Keyboard {
        let sink = self.parser.callbacks();
        let screen = self.parser.screen();
        crate::input::Keyboard {
            kitty: sink.kitty[usize::from(screen.alternate_screen())]
                .last()
                .copied()
                .unwrap_or(0),
            modify_other_keys: sink.modify_other_keys,
        }
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

    /// The program the pane was started with, used as its title until it
    /// says otherwise.
    pub fn program(&self) -> &str {
        &self.name
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

    /// The pane's text, for `capture-pane`. With `history`, the scrollback
    /// comes first: one line per step back, because a screenful at a time
    /// would double up wherever the history is not a whole number of screens.
    pub fn dump(&mut self, history: bool) -> String {
        let mut lines: Vec<String> = vec![];
        if history {
            self.parser.screen_mut().set_scrollback(usize::MAX);
            let oldest = self.parser.screen().scrollback();
            for at in (1..=oldest).rev() {
                self.parser.screen_mut().set_scrollback(at);
                if let Some(row) = self.parser.screen().rows(0, self.cols).next() {
                    lines.push(row);
                }
            }
            // The history ends where the live screen begins, whatever the
            // view is scrolled to.
            self.parser.screen_mut().set_scrollback(0);
        }
        lines.extend(self.parser.screen().rows(0, self.cols));
        self.parser.screen_mut().set_scrollback(self.scroll);
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// Rows that have left the top of the screen, the origin copy mode counts
    /// its lines from.
    pub fn scrolled_off(&self) -> usize {
        self.parser.screen().scrolled_off()
    }

    /// The text from `from` to `to` inclusive, each a (line, column) where
    /// line 0 is the top of the live screen and history is negative. A row
    /// that wrapped joins the next without a newline.
    pub fn text_between(&mut self, from: (isize, u16), to: (isize, u16)) -> String {
        let (from, to) = if from <= to { (from, to) } else { (to, from) };
        let mut out = String::new();
        // Lines that have fallen out of the history since the selection began
        // are gone; asking for them would repeat the oldest one.
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let oldest = -(self.parser.screen().scrollback() as isize);
        for line in from.0.max(oldest)..=to.0 {
            let back = (-line).max(0) as usize;
            self.parser.screen_mut().set_scrollback(back);
            let row = (line + back as isize) as u16;
            let start = if line == from.0 { from.1 } else { 0 };
            let end = if line == to.0 { to.1 + 1 } else { self.cols };
            let screen = self.parser.screen();
            let text = screen
                .rows(start, end.saturating_sub(start))
                .nth(row.into())
                .unwrap_or_default();
            if line == to.0 || screen.row_wrapped(row) {
                out.push_str(&text);
            } else {
                out.push_str(text.trim_end());
                out.push('\n');
            }
        }
        self.parser.screen_mut().set_scrollback(self.scroll);
        out
    }

    /// Send `text` to the outer terminal's clipboard with OSC 52, through the
    /// same queue as a program's own copies, so `general.clipboard` gates both.
    pub fn copy_to_host(&mut self, text: &str) {
        let sink = self.parser.callbacks_mut();
        sink.copies.extend_from_slice(b"\x1b]52;c;");
        sink.copies
            .extend_from_slice(base64(text.as_bytes()).as_bytes());
        sink.copies.extend_from_slice(b"\x1b\\");
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
        // 0 is *this* process's group and 1 is launchd/init: a snapshot
        // that lost a pid must not turn into a signal aimed at us.
        #[cfg(unix)]
        let pg = child
            .process_id()
            .map(|p| p as libc::pid_t)
            .filter(|pg| *pg > 1);
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
    fn copy_mode_text_spans_history_and_screen() {
        let mut p = pane(
            "i=1; while [ $i -le 30 ]; do echo line$i; i=$((i+1)); done",
            40,
            10,
        );
        assert!(pump_until(&mut p, |p| p
            .screen()
            .contents()
            .contains("line30")));
        let top = (0..10)
            .find(|&r| p.screen().rows(0, 40).nth(r).unwrap() == "line22")
            .unwrap() as isize;
        // Two lines above the screen, through the first on it, part-way in.
        assert_eq!(p.text_between((top - 2, 4), (top, 4)), "20\nline21\nline2");
        assert_eq!(p.text_between((top, 1), (top - 1, 0)), "line21\nli");
        assert_eq!(p.scroll, 0);
        assert_eq!(base64(b"hi there"), "aGkgdGhlcmU=");
        assert_eq!(base64(b"ab"), "YWI=");
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
        let mut p = Pane::spawn(1, &cfg, None, None, 40, 4, &[]).unwrap();
        assert!(pump_until(&mut p, |p| p
            .screen()
            .contents()
            .contains("truecolor")));
    }

    #[test]
    fn a_graphics_query_is_answered_here_and_not_passed_on() {
        // Asks, then prints what came back with the escapes made visible.
        let mut p = pane(
            "printf '\\033_Ga=q,i=31,s=1,v=1,f=24;AAAA\\033\\\\';              head -c 18 | tr -d '\\033\\\\'",
            40,
            6,
        );
        assert!(pump_until(&mut p, |p| p
            .screen()
            .contents()
            .contains("_Gi=31;OK")));
        assert!(p.take_images().is_empty(), "the query went to the terminal");
    }

    #[test]
    fn a_reset_after_scrolling_does_not_panic() {
        let mut p = pane(
            "seq 1 50; sleep 0.3; printf '\\033c'; echo after''-reset",
            20,
            5,
        );
        assert!(pump_until(&mut p, |p| p.screen().contents().contains("50")));
        assert!(pump_until(&mut p, |p| p
            .screen()
            .contents()
            .contains("after-reset")));
    }

    #[test]
    fn scrolled_off_never_goes_down_into_the_alternate_screen() {
        let mut p = vt100::Parser::new(4, 10, 100);
        p.process("1\r\n2\r\n3\r\n4\r\n5\r\n6".as_bytes());
        let before = p.screen().scrolled_off();
        assert!(before > 0);
        p.process(b"\x1b[?1049h");
        assert!(p.screen().scrolled_off() >= before);
    }

    #[test]
    fn a_name_the_last_client_did_not_have_is_gone_from_a_new_pane() {
        let mut cfg = Config::default();
        cfg.general.shell = "/bin/sh".into();
        cfg.general.shell_args = vec!["-c".into(), "printf '[%s|%s]' \"$HOME\" \"$A\"".into()];
        let env = [("HOME".into(), None), ("A".into(), Some("b".into()))];
        let mut p = Pane::spawn(1, &cfg, None, None, 40, 4, &env).unwrap();
        assert!(pump_until(&mut p, |p| p
            .screen()
            .contents()
            .contains("[|b]")));
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
        // Upstream vt100 implements only CUP and drops HVP, so mpv's
        // `--vo=tct` frames, which move with `f`, paint as one wrapping stream.
        let mut p = pane("printf '\\033[3;5fX\\033[0;1fY'", 40, 10);
        assert!(pump_until(&mut p, |p| p.screen().contents().contains('X')));
        assert_eq!(p.screen().cell(2, 4).map(|c| c.contents()), Some("X"));
        // A row or column of 0 means 1, same as for CUP.
        assert_eq!(p.screen().cell(0, 0).map(|c| c.contents()), Some("Y"));
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
            "i=0; while [ $i -lt 40 ]; do printf '\\033_Gn=%s;\\033\\\\x' $i; \
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

    #[test]
    fn cursor_and_status_queries_are_answered() {
        let mut p = vt100::Parser::new_with_callbacks(10, 20, 0, Sink::default());
        p.process(b"\x1b[3;5H\x1b[6n\x1b[5n\x1b[c\x1b[?6n");
        assert_eq!(p.callbacks().replies, b"\x1b[3;5R\x1b[0n\x1b[?62;22;52c");
    }

    #[test]
    fn modes_vt100_ignores_are_tracked_and_reported() {
        let mut p = vt100::Parser::new_with_callbacks(10, 20, 0, Sink::default());
        p.process(b"\x1b[?1004;2004h\x1b[?1004$p\x1b[?2004$p\x1b[?2026$p\x1b[?7777$p");
        assert!(p.callbacks().focus_events);
        assert_eq!(
            String::from_utf8_lossy(&p.callbacks().replies),
            "\x1b[?1004;1$y\x1b[?2004;1$y\x1b[?2026;2$y\x1b[?7777;0$y"
        );
        p.callbacks_mut().replies.clear();
        p.process(b"\x1b[?1004l\x1b[5 q\x1b[>c\x1b[>q\x1b[18t");
        assert!(!p.callbacks().focus_events);
        assert_eq!(p.callbacks().cursor_shape, 5);
        let r = String::from_utf8_lossy(&p.callbacks().replies).to_string();
        assert!(r.starts_with("\x1b[>1;10;0c\x1bP>|ttmux "), "{r:?}");
        assert!(r.ends_with("\x1b\\\x1b[8;10;20t"), "{r:?}");
    }

    #[test]
    fn kitty_keyboard_stacks_are_per_screen_and_queries_see_them() {
        let mut p = vt100::Parser::new_with_callbacks(10, 20, 0, Sink::default());
        // Push 1, then 31 of which only 1|8 is honoured; query; pop one.
        p.process(b"\x1b[>1u\x1b[>31u\x1b[?u\x1b[<u\x1b[?u");
        assert_eq!(p.callbacks().replies, b"\x1b[?9u\x1b[?1u");
        p.callbacks_mut().replies.clear();
        // The alternate screen starts clean; `=` sets, ORs and clears there.
        p.process(b"\x1b[?1049h\x1b[?u\x1b[=8u\x1b[=1;2u\x1b[=8;3u\x1b[?u");
        assert_eq!(p.callbacks().replies, b"\x1b[?0u\x1b[?1u");
        p.callbacks_mut().replies.clear();
        // Leaving it finds the main screen's stack as it was.
        p.process(b"\x1b[?1049l\x1b[?u\x1b[<5u\x1b[?u\x1b[>4;2m");
        assert_eq!(p.callbacks().replies, b"\x1b[?1u\x1b[?0u");
        assert_eq!(p.callbacks().modify_other_keys, 2);
        p.process(b"\x1b[>4m");
        assert_eq!(p.callbacks().modify_other_keys, 0);
    }

    #[test]
    fn xtgettcap_answers_known_names_and_refuses_the_rest() {
        let mut p = vt100::Parser::new_with_callbacks(10, 20, 0, Sink::default());
        // "Tc", "Co" and "xx", hex encoded.
        p.process(b"\x1bP+q5463;436F;7878\x1b\\");
        assert_eq!(
            String::from_utf8_lossy(&p.callbacks().replies),
            "\x1bP1+r5463\x1b\\\x1bP1+r436F=323536\x1b\\\x1bP0+r7878\x1b\\"
        );
        assert_eq!(termcap("Se"), Some("\\E[2 q"));
    }

    #[test]
    fn theme_queries_answer_from_the_background() {
        assert_eq!(
            theme_of(&Some(("".into(), "rgb:1a1a/1b1b/2626".into()))),
            Some(1)
        );
        assert_eq!(
            theme_of(&Some(("".into(), "rgb:fafa/f8f8/f0f0".into()))),
            Some(2)
        );
        assert_eq!(theme_of(&None), None);
        let mut p = vt100::Parser::new_with_callbacks(10, 20, 0, Sink::default());
        p.process(b"\x1b[?996n");
        assert!(p.callbacks().replies.is_empty(), "unknown until told");
        p.callbacks_mut().colours = Some(("rgb:0/0/0".into(), "rgb:ffff/ffff/ffff".into()));
        p.process(b"\x1b[?996n\x1b[?2031h\x1b[?2031$p");
        assert_eq!(p.callbacks().replies, b"\x1b[?997;2n\x1b[?2031;1$y");
    }

    #[test]
    fn clipboard_notifications_and_colours_reach_the_right_side() {
        let mut p = vt100::Parser::new_with_callbacks(10, 20, 0, Sink::default());
        p.process(b"\x1b]52;c;aGk=\x07\x1b]777;notify;done;ok\x07\x1b]11;?\x07");
        assert_eq!(p.callbacks().copies, b"\x1b]52;c;aGk=\x1b\\");
        assert_eq!(p.callbacks().notices, b"\x1b]777;notify;done;ok\x1b\\");
        assert!(p.callbacks().replies.is_empty(), "no colours known yet");
        p.callbacks_mut().colours =
            Some(("rgb:ffff/ffff/ffff".into(), "rgb:0000/0000/0000".into()));
        p.process(b"\x1b]11;?\x07\x1b]52;c;?\x07");
        assert_eq!(
            String::from_utf8_lossy(&p.callbacks().replies),
            "\x1b]11;rgb:0000/0000/0000\x07\x1b]52;c;aGk=\x1b\\"
        );
    }
}

// ----------------------------------------------------- handover

/// A pty master that arrived over a socket rather than from `openpty`.
///
/// The pty itself does not care who holds its master: the shell's session and
/// controlling terminal were fixed when it was spawned and are not affected
/// by this end changing hands. All that matters is that *someone* holds the
/// master at every instant, or the kernel hangs the shell up.
#[derive(Debug)]
struct AdoptedMaster {
    fd: OwnedFd,
}

impl MasterPty for AdoptedMaster {
    fn resize(&self, size: PtySize) -> anyhow::Result<()> {
        let ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCSWINSZ, &ws) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn get_size(&self) -> anyhow::Result<PtySize> {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(PtySize {
            rows: ws.ws_row,
            cols: ws.ws_col,
            pixel_width: ws.ws_xpixel,
            pixel_height: ws.ws_ypixel,
        })
    }

    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send>> {
        Ok(Box::new(std::fs::File::from(self.fd.try_clone()?)))
    }

    fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send>> {
        Ok(Box::new(std::fs::File::from(self.fd.try_clone()?)))
    }

    fn process_group_leader(&self) -> Option<libc::pid_t> {
        match unsafe { libc::tcgetpgrp(self.fd.as_raw_fd()) } {
            -1 => None,
            pg => Some(pg),
        }
    }

    fn as_raw_fd(&self) -> Option<RawFd> {
        Some(self.fd.as_raw_fd())
    }

    fn tty_name(&self) -> Option<PathBuf> {
        None
    }
}

/// A shell this process did not spawn, so `wait` is not available: it is
/// watched with signal 0 instead, and its exit status is unknowable.
#[derive(Debug, Clone)]
struct AdoptedChild {
    pid: libc::pid_t,
    /// Once it is gone the pid must never be signalled again: the kernel is
    /// free to hand that number to somebody else.
    gone: Arc<AtomicBool>,
}

impl AdoptedChild {
    fn alive(&self) -> bool {
        if self.gone.load(Ordering::Relaxed) {
            return false;
        }
        if unsafe { libc::kill(self.pid, 0) } == 0 {
            return true;
        }
        // EPERM means alive but not ours; only ESRCH means gone.
        let gone = std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            self.gone.store(true, Ordering::Relaxed);
        }
        !gone
    }
}

impl ChildKiller for AdoptedChild {
    fn kill(&mut self) -> std::io::Result<()> {
        if self.alive() {
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
        }
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(self.clone())
    }
}

impl Child for AdoptedChild {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        // An adopted shell is reaped by launchd, not by us, so there is no
        // status to collect: "it ended" is all this can ever say.
        Ok((!self.alive()).then(|| ExitStatus::with_exit_code(0)))
    }

    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        // Bounded: `alive` calls anything but ESRCH alive, so a pid that has
        // been recycled by a process this user cannot signal would otherwise
        // spin here for ever -- inside `Drop`, on the app thread.
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(ExitStatus::with_exit_code(0))
    }

    fn process_id(&self) -> Option<u32> {
        // `None` once it is gone, which is what stops `Pane::kill` from
        // signalling a recycled pid.
        self.alive().then_some(self.pid as u32)
    }
}

impl Pane {
    /// Everything the new server needs to keep this pane: the pty master, and
    /// escape codes that rebuild what is on it.
    ///
    /// Takes `&mut self` because draining what has arrived but not yet been
    /// drawn is part of the picture -- the bytes are in this process's
    /// channel, not in the kernel, and would otherwise be lost.
    pub fn handover(&mut self) -> anyhow::Result<(u32, OwnedFd, Vec<u8>)> {
        let pid = self
            .child
            .borrow()
            .process_id()
            .context("pane has no process")?;
        let fd = self.master.as_raw_fd().context("pane has no pty")?;
        // Duplicated before anything is closed, so the master is held by one
        // process or the other without a gap. A gap would hang up the shell.
        // Close-on-exec, or a widget command spawned before this process
        // exits would inherit every pane's master.
        let dup = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }
            .try_clone_to_owned()
            .context("dup pty")?;
        // ponytail: the reader thread here keeps reading until this process
        // exits, so output from this drain to that exit is lost, and a cut
        // can land mid escape sequence. The window is the fd passing plus the
        // new server's parse of the snapshot. Closing it means a shutdown
        // channel per pane.
        //
        // Bounded, not `while self.pump()`: a pane running `yes` is never
        // drained, and looping until it is would hang the app thread.
        for _ in 0..8 {
            if !self.pump() {
                break;
            }
        }
        Ok((pid, dup, self.replay()))
    }

    /// Escape codes that reproduce this pane: scrollback as plain lines, then
    /// the live screen with its colours and modes.
    ///
    /// The scrollback loses its formatting on the way through, which is the
    /// trade for it surviving at all: the alternative is a row-by-row dump of
    /// ten thousand lines of attributes.
    fn replay(&mut self) -> Vec<u8> {
        let mut out: Vec<u8> = vec![];
        let history = self.parser.screen().history();
        for line in &history {
            out.extend_from_slice(line.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        if !history.is_empty() {
            // Those lines land on the screen first and only scroll into the
            // history as more arrive. The cursor is on the last row, so one
            // newline fewer than a screenful pushes the last of them off.
            out.extend(std::iter::repeat_n(b'\n', usize::from(self.rows) - 1));
        }
        // Kitty keyboard flags are a stack per screen: the primary's is
        // pushed before any switch, the alternate's after it.
        let kitty = self.parser.callbacks().kitty.clone();
        for flags in &kitty[0] {
            out.extend_from_slice(format!("\x1b[>{flags}u").as_bytes());
        }
        // A pane in the alternate screen (vim, less, top) must be restored
        // onto the alternate grid, or its next `?1049l` would restore a grid
        // it never left and paint the shell's prompt into the leftovers.
        //
        // ponytail: what the primary grid *showed* underneath is lost, and
        // so are the saved cursor and the charset; carrying them means
        // dumping a second grid and more of vt100's private state.
        let alt = self.parser.screen().alternate_screen();
        if alt {
            out.extend_from_slice(b"\x1b[?1049h");
            for flags in &kitty[1] {
                out.extend_from_slice(format!("\x1b[>{flags}u").as_bytes());
            }
        }
        // The live screen, not whatever is scrolled back to -- or frozen in
        // copy mode -- right now.
        let here = self.parser.screen().scrollback();
        self.parser.screen_mut().set_scrollback(0);
        out.extend_from_slice(&self.parser.screen().state_formatted());
        // A pinned footer (apt's progress bar) is a scroll region. Set after
        // the screen, whose row-to-row newlines would scroll inside it, and
        // then put back the cursor that setting one homes.
        let (top, bottom) = self.parser.screen().scroll_region();
        if (top, bottom) != (0, self.rows - 1) {
            let (row, col) = self.parser.screen().cursor_position();
            out.extend_from_slice(
                format!(
                    "\x1b[{};{}r\x1b[{};{}H",
                    top + 1,
                    bottom + 1,
                    row + 1,
                    col + 1
                )
                .as_bytes(),
            );
        }
        self.parser.screen_mut().set_scrollback(here);
        // What the child negotiated about *input*, which the screen dump does
        // not carry: a program already in one of these modes will never ask
        // again.
        let sink = self.parser.callbacks();
        if sink.modify_other_keys != 0 {
            out.extend_from_slice(format!("\x1b[>4;{}m", sink.modify_other_keys).as_bytes());
        }
        if sink.focus_events {
            out.extend_from_slice(b"\x1b[?1004h");
        }
        if sink.theme_reports {
            out.extend_from_slice(b"\x1b[?2031h");
        }
        if sink.cursor_shape != 0 {
            out.extend_from_slice(format!("\x1b[{} q", sink.cursor_shape).as_bytes());
        }
        if !sink.title.is_empty() {
            out.extend_from_slice(format!("\x1b]2;{}\x07", sink.title).as_bytes());
        }
        out
    }

    /// Rebuild a pane around a pty master handed over by another server.
    pub fn adopt(
        snap: &crate::migrate::PaneSnap,
        fd: OwnedFd,
        scrollback: usize,
    ) -> anyhow::Result<Pane> {
        if snap.pid <= 1 {
            anyhow::bail!("pane {} arrived without a pid", snap.id);
        }
        let (cols, rows) = (snap.cols.max(1), snap.rows.max(1));
        let mut parser = vt100::Parser::new_with_callbacks(rows, cols, scrollback, Sink::default());
        parser.process(&snap.replay);
        // The replay is this pane's own past, not new output: anything it
        // emitted -- a title, a clipboard copy, a bell -- has already been
        // acted on once.
        parser.callbacks_mut().copies.clear();
        parser.callbacks_mut().notices.clear();
        let child = AdoptedChild {
            pid: snap.pid as libc::pid_t,
            gone: Arc::new(AtomicBool::new(false)),
        };
        Pane::assemble(
            snap.id,
            snap.name.clone(),
            parser,
            Box::new(AdoptedMaster { fd }),
            Box::new(child),
        )
    }
}
