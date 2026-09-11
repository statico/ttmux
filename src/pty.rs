//! One pseudo-terminal per pane: child process, terminal emulator, scrollback.
//!
//! Output is read on a background thread into a channel; [`Pane::pump`] drains
//! that channel into the emulator and never blocks.

use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use anyhow::Context;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

use crate::config::Config;
use crate::layout::PaneId;

/// Read buffer for the reader thread.
const READ_BUF: usize = 64 * 1024;
/// Most bytes one `pump()` will feed the emulator, so a noisy pane can't
/// starve the UI.
const PUMP_BUDGET: usize = 4 * 1024 * 1024;

/// Emulator callbacks: everything vt100 reports outside the screen grid.
#[derive(Default)]
struct Sink {
    title: String,
    bell: bool,
}

impl vt100::Callbacks for Sink {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bell = true;
    }
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
    }
}

/// A single terminal pane.
pub struct Pane {
    pub id: PaneId,
    pub cols: u16,
    pub rows: u16,
    /// Rows scrolled back; 0 = live.
    pub scroll: usize,
    /// Sticky bell flag, cleared by the app.
    pub bell: bool,
    pub title_override: Option<String>,

    parser: vt100::Parser<Sink>,
    rx: Receiver<Vec<u8>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: RefCell<Box<dyn Child + Send + Sync>>,
    exit: Cell<Option<u32>>,
    /// Reader thread gone (EOF or I/O error).
    disconnected: bool,
    /// Fallback title: the program's basename.
    name: String,
}

impl Pane {
    /// Spawn the configured shell in a new pty.
    pub fn spawn(
        id: PaneId,
        cfg: &Config,
        cwd: Option<PathBuf>,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<Pane> {
        let shell = if !cfg.general.shell.is_empty() {
            cfg.general.shell.clone()
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
        };
        let mut cmd = CommandBuilder::new(&shell);
        cmd.args(&cfg.general.shell_args);
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("TTMUX", "1");
        cmd.env("TTMUX_PANE", id.to_string());
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
        let name = cmd
            .get_argv()
            .first()
            .map(|p| {
                PathBuf::from(p)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .unwrap_or_default();

        let pair = portable_pty::native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("openpty")?;
        let child = pair.slave.spawn_command(cmd).context("spawn")?;
        // Drop our copy of the slave so the reader sees EOF when the child exits.
        drop(pair.slave);

        let writer = pair.master.take_writer().context("pty writer")?;
        let mut reader = pair.master.try_clone_reader().context("pty reader")?;
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

        Ok(Pane {
            id,
            cols,
            rows,
            scroll: 0,
            bell: false,
            title_override: None,
            parser: vt100::Parser::new_with_callbacks(rows, cols, scrollback, Sink::default()),
            rx,
            master: pair.master,
            writer,
            child: RefCell::new(child),
            exit: Cell::new(None),
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
        let _ = self
            .master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
    }

    /// Write bytes to the child. I/O errors mark the pane dead.
    pub fn send(&mut self, bytes: &[u8]) {
        if self.writer.write_all(bytes).is_err() || self.writer.flush().is_err() {
            self.disconnected = true;
        }
    }

    /// Feed pending child output into the emulator. True if anything changed.
    pub fn pump(&mut self) -> bool {
        if self.disconnected {
            return false;
        }
        let mut got = 0usize;
        while got < PUMP_BUDGET {
            match self.rx.try_recv() {
                Ok(chunk) => {
                    got += chunk.len();
                    self.parser.process(&chunk);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.disconnected = true;
                    break;
                }
            }
        }
        if got == 0 {
            return false;
        }
        if std::mem::take(&mut self.parser.callbacks_mut().bell) {
            self.bell = true;
        }
        // New output only reaches the live view; keep the scrollback offset put.
        self.parser.screen_mut().set_scrollback(self.scroll);
        true
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

    /// Kill the child; the reader thread exits on its own at EOF.
    pub fn kill(&mut self) {
        let _ = self.child.borrow_mut().kill();
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
        assert!(pump_until(&mut p, |p| p.screen().contents().contains("hello")));
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
        assert!(pump_until(&mut p, |p| p.screen().contents().contains("got:abc")));
    }

    #[test]
    fn resize_changes_the_emulator_size() {
        let mut p = pane("printf hi; sleep 1", 40, 10);
        assert!(pump_until(&mut p, |p| p.screen().contents().contains("hi")));
        p.resize(60, 20);
        assert_eq!(p.screen().size(), (20, 60));
        assert_eq!((p.cols, p.rows), (60, 20));
        p.pump();
    }

    #[test]
    fn scrollback_moves_the_view() {
        let mut p = pane("i=1; while [ $i -le 200 ]; do echo line$i; i=$((i+1)); done", 40, 10);
        assert!(pump_until(&mut p, |p| p.screen().contents().contains("line200")));
        let live = p.screen().contents();
        p.scroll_by(-5);
        assert_eq!(p.scroll, 5);
        assert_ne!(p.screen().contents(), live);
        p.scroll_to_bottom();
        assert_eq!(p.screen().contents(), live);
    }

    #[test]
    fn tail_returns_the_last_lines() {
        let mut p = pane("printf 'a\\nb\\nc\\nd\\n'", 40, 10);
        assert!(pump_until(&mut p, |p| p.screen().contents().contains('d')));
        let t = p.tail(3);
        assert_eq!(t.lines().map(str::trim).collect::<Vec<_>>(), ["b", "c", "d"]);
    }

    #[test]
    fn osc_sets_the_title() {
        let mut p = pane("printf '\\033]0;mytitle\\007'", 40, 10);
        assert!(pump_until(&mut p, |p| p.title() == "mytitle"));
    }
}
