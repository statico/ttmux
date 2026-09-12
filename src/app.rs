//! The application: terminal setup, the event loop, and action dispatch.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::{cursor::MoveTo, execute, queue, terminal};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Terminal;

use crate::action::{Action, Dir, ALL_ACTIONS};
use crate::agent::{AgentState, Watcher};
use crate::cmdline::{CmdLine, Outcome as CmdOutcome};
use crate::config::{Bar, BorderStyle, Config};
use crate::graphics::Image;
use crate::input::{encode_key, encode_mouse, Keys, Resolution};
use crate::layout::{Layout, Mode, PaneId, Preset, Rect};
use crate::line_edit::{LineEdit, CARET};
use crate::pty::Pane;
use crate::render;
use crate::script::Cmd;
use crate::settings_ui::{Outcome, Settings};
use crate::status;
use crate::widget;

/// How long a transient status message stays up.
const MESSAGE_TTL: Duration = Duration::from_secs(4);
/// Event-loop tick while something is happening. The loop cannot wait on the
/// terminal and on the panes at once, so this bounds how long a keystroke and
/// the program's answer to it each sit unnoticed.
const TICK_BUSY: Duration = Duration::from_millis(1);
/// The tick once everything has been quiet, where a wakeup is only battery.
const TICK_IDLE: Duration = Duration::from_millis(16);
/// How long after the last keystroke or byte the loop stays on the fast tick.
const BUSY_FOR: Duration = Duration::from_millis(400);

/// One scripted command waiting for the app thread, with the channel its
/// answer goes back on.
pub struct ScriptJob {
    pub cmd: Cmd,
    /// The pane the script was run from, if any. A command with no target
    /// acts on it rather than on the focused pane.
    pub caller: Option<PaneId>,
    pub reply: std::sync::mpsc::SyncSender<Result<String, String>>,
}

/// Where a running `App` gets its input and where out-of-band bytes go.
pub trait Host {
    /// Wait up to `timeout` for one event. `Ok(None)` means the timeout
    /// elapsed; `Err` means the source is gone and the loop should end.
    fn poll(&mut self, timeout: Duration) -> Result<Option<Event>>;
    /// Bytes for the attached terminal verbatim -- inline-image replays and
    /// the bell. These cannot go through the cell buffer.
    fn passthrough(&mut self, bytes: &[u8]) -> Result<()>;
    /// Scripted commands that have arrived since the last call. Only the
    /// session server has a socket for them, so the default is none.
    fn commands(&mut self) -> Vec<ScriptJob> {
        Vec::new()
    }
}

/// The local terminal: crossterm's event queue and this process's stdout.
pub struct LocalHost;

impl Host for LocalHost {
    fn poll(&mut self, timeout: Duration) -> Result<Option<Event>> {
        if event::poll(timeout)? {
            Ok(Some(event::read()?))
        } else {
            Ok(None)
        }
    }

    fn passthrough(&mut self, bytes: &[u8]) -> Result<()> {
        let mut out = io::stdout();
        out.write_all(bytes)?;
        out.flush()?;
        Ok(())
    }
}

/// Why `main_loop` returned. The driver decides what that costs the session:
/// `Quit` ends it and kills every pane; `Detached` leaves the panes running
/// and only drops this view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Quit,
    Detached,
}

// ------------------------------------------------------------------ state

struct Slot {
    pane: Pane,
    watcher: Watcher,
    state: AgentState,
    /// Graphics the pane has emitted. A program draws an image once, so ttmux
    /// has to keep the sequence and re-emit it every frame.
    images: Vec<Image>,
}

struct Tab {
    name: String,
    layout: Layout,
    focus: PaneId,
    /// Set when the user has renamed it, so it stops tracking the pane title.
    renamed: bool,
}

enum Overlay {
    None,
    Help {
        scroll: usize,
    },
    Settings(Settings),
    Palette {
        query: LineEdit,
        sel: usize,
    },
    Prompt {
        label: String,
        input: LineEdit,
        target: Rename,
    },
    Welcome(crate::onboarding::Welcome),
    Command(CmdLine),
}

/// Pretty JSON with a trailing newline, so a script can pipe it straight
/// into `jq` and a human reading it gets a line break.
fn json_line(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default() + "\n"
}

/// The TOML value a `set-option` argument means. A shell has only strings,
/// so the type comes from what the word looks like.
fn parse_scalar(word: &str) -> toml::Value {
    if let Ok(b) = word.parse::<bool>() {
        return toml::Value::Boolean(b);
    }
    if let Ok(i) = word.parse::<i64>() {
        return toml::Value::Integer(i);
    }
    if let Ok(f) = word.parse::<f64>() {
        return toml::Value::Float(f);
    }
    toml::Value::String(word.to_string())
}

/// What a rename prompt is about to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rename {
    Tab,
    Pane(PaneId),
    /// Not a rename: the number of the tab to move this pane into. It shares
    /// the prompt because the prompt is just "ask for one line".
    JoinTo(PaneId),
}

pub struct App {
    cfg: Config,
    cfg_path: PathBuf,
    /// The pane the running scripted command came from, for the length of
    /// that command. See [`App::script_from`].
    caller: Option<PaneId>,
    /// The config file's mtime as of the last load, so an edit on disk can be
    /// noticed without polling the contents.
    cfg_text: String,
    widgets: widget::Runner,
    keys: Keys,
    tabs: Vec<Tab>,
    tab: usize,
    /// Where `last-tab` goes back to, tmux's `last-window`.
    last_tab: Option<usize>,
    slots: HashMap<PaneId, Slot>,
    next_id: PaneId,
    overlay: Overlay,
    /// Lines run on the `:` line, oldest first, so Up recalls across opens.
    cmd_history: Vec<String>,
    message: Option<(String, Instant)>,
    /// Hitboxes published by the last status draw, for mouse clicks. Both
    /// rows can list tabs, so the row is part of the box.
    tab_hits: Vec<(usize, u16, std::ops::Range<u16>)>,
    area: Rect,
    session: String,
    /// Whether the last frame emitted any graphics, so a frame with none
    /// still issues one delete pass to wipe what the last one placed.
    drew_graphics: bool,
    /// Set by the `quit` action: tear the session down, panes included.
    pub quit: bool,
    /// Set by the `detach` action: end this view only, panes keep running.
    pub detached: bool,
}

// ------------------------------------------------------------------ entry

pub fn run() -> Result<()> {
    let path = crate::config::config_path();
    let (loaded, cfg_seen) = Config::load_stamped(&path);
    let cfg = loaded.unwrap_or_else(|e| {
        eprintln!("ttmux: {}: {e}; using defaults", path.display());
        Config::default()
    });

    let mut term = setup(&cfg)?;
    // Always restore the terminal, even on a panic, or the user is left with a
    // dead raw-mode shell.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        hook(info);
    }));

    let result = (|| {
        let size = term.size()?;
        let mut app = App::new(
            cfg,
            path,
            Rect::new(0, 0, size.width, size.height),
            cfg_seen,
        )?;
        app.main_loop(&mut term, &mut LocalHost)?;
        Ok(())
    })();
    restore()?;
    result
}

fn setup(cfg: &Config) -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    terminal::enable_raw_mode()?;
    enter(cfg).inspect_err(|_| {
        let _ = restore();
    })
}

fn enter(cfg: &Config) -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    let mut out = io::stdout();
    execute!(out, terminal::EnterAlternateScreen)?;
    if cfg.general.mouse {
        execute!(out, event::EnableMouseCapture)?;
    }
    execute!(out, event::EnableBracketedPaste)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn restore() -> Result<()> {
    let mut out = io::stdout();
    let _ = execute!(
        out,
        event::DisableBracketedPaste,
        event::DisableMouseCapture,
        terminal::LeaveAlternateScreen
    );
    terminal::disable_raw_mode()?;
    let _ = out.flush();
    Ok(())
}

// ------------------------------------------------------------------- app

impl App {
    /// `cfg_text` is the text the config was parsed from, from
    /// [`Config::load_stamped`]. Reading the file here instead would miss an
    /// edit made between the caller's read and this call, and never reload.
    pub fn new(cfg: Config, cfg_path: PathBuf, area: Rect, cfg_text: String) -> Result<App> {
        let keys = Keys::new(&cfg);
        // No config file means nobody has chosen a keymap yet, so offer the
        // choice before the first keystroke lands on a default they did not
        // pick. Answering it writes the file, which is what retires this.
        let first_run = !cfg_path.exists();
        let mut app = App {
            keys,
            cfg_text,
            widgets: widget::Runner::default(),
            cfg,
            cfg_path,
            tabs: vec![],
            tab: 0,
            last_tab: None,
            slots: HashMap::new(),
            next_id: 1,
            overlay: Overlay::None,
            cmd_history: vec![],
            message: None,
            tab_hits: vec![],
            area,
            session: std::env::var("TTMUX_SESSION").unwrap_or_else(|_| "main".into()),
            caller: None,
            drew_graphics: false,
            quit: false,
            detached: false,
        };
        // Not just on reload: a widget that is in the config at startup has
        // to run too.
        app.widgets.reload(&app.cfg.status.widgets);
        app.new_tab()?;
        if first_run {
            app.overlay = Overlay::Welcome(crate::onboarding::Welcome::new());
        }
        Ok(app)
    }

    // ---------------------------------------------------------- geometry

    /// The rect panes live in: everything the status rows do not take.
    fn body(&self) -> Rect {
        let a = self.area;
        let top = self.header_rect().is_some() as u16;
        let bottom = self.footer_rect().is_some() as u16;
        Rect::new(a.x, a.y + top, a.w, a.h - top - bottom)
    }

    /// The two status rows, either, both or neither.
    ///
    /// A row is only given away if a pane still has a line to live on, so a
    /// two-row terminal keeps its pane instead of becoming all status bar.
    fn header_rect(&self) -> Option<Rect> {
        let a = self.area;
        (self.cfg.status.header.enabled && a.h >= 2).then(|| Rect::new(a.x, a.y, a.w, 1))
    }

    fn footer_rect(&self) -> Option<Rect> {
        let a = self.area;
        let room = if self.cfg.status.header.enabled { 3 } else { 2 };
        (self.cfg.status.footer.enabled && a.h >= room)
            .then(|| Rect::new(a.x, a.y + a.h - 1, a.w, 1))
    }

    /// Each enabled row with the widget list it draws, top first.
    fn status_rows(&self) -> Vec<(Rect, &Bar)> {
        [
            self.header_rect().map(|r| (r, &self.cfg.status.header)),
            self.footer_rect().map(|r| (r, &self.cfg.status.footer)),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// A pane's tile split into the rect the border is drawn on and the rect
    /// its screen is drawn in. `inner` is also what the pty is sized to, so
    /// these must come from one place or the content is clipped and the mouse
    /// coordinates are off.
    fn frame(&self, id: PaneId, outer: Rect) -> (Rect, Rect) {
        (outer.shrink(self.cfg.appearance.gap), self.inner(id, outer))
    }

    /// The drawable interior of a pane: outer rect minus gap and border.
    fn inner(&self, id: PaneId, outer: Rect) -> Rect {
        let outer = outer.shrink(self.cfg.appearance.gap);
        match self.cfg.appearance.border_style {
            BorderStyle::None => outer,
            // A divider costs the pane only the sides it draws one on, so a
            // pane against the edge of the screen keeps those rows.
            BorderStyle::Divider if !self.tabs[self.tab].layout.is_floating(id) => {
                let (left, top) = self.divider_sides(outer);
                Rect::new(
                    outer.x + left as u16,
                    outer.y + top as u16,
                    outer.w.saturating_sub(left as u16),
                    outer.h.saturating_sub(top as u16),
                )
            }
            _ => outer.shrink(1),
        }
    }

    /// Which sides of a pane face another pane rather than the edge of the
    /// tab, and so carry its share of the dividers.
    fn divider_sides(&self, outer: Rect) -> (bool, bool) {
        let area = self.tabs[self.tab].layout.area();
        (outer.x > area.x, outer.y > area.y)
    }

    /// Focus a pane in the current tab. A zoomed pane hides every other pane,
    /// so moving focus off it must unzoom, or keystrokes go to something the
    /// user cannot see.
    fn set_focus(&mut self, id: PaneId) {
        let t = self.tab_mut();
        t.focus = id;
        if t.layout.zoomed.is_some_and(|z| z != id) {
            t.layout.set_zoom(None);
            self.sync_sizes();
        }
    }

    fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.tab]
    }

    fn focus(&self) -> PaneId {
        self.tabs[self.tab].focus
    }

    /// Push the current geometry down to the ptys. Cheap no-op when unchanged.
    fn sync_sizes(&mut self) {
        let geo = self.tabs[self.tab].layout.geometry();
        for (id, outer) in geo {
            let r = self.inner(id, outer);
            if let Some(slot) = self.slots.get_mut(&id) {
                let (cols, rows) = (r.w.max(1), r.h.max(1));
                if slot.pane.cols != cols || slot.pane.rows != rows {
                    slot.pane.resize(cols, rows);
                    // Reflow moves every line, so the cells the standing
                    // images were captured at no longer mean anything.
                    slot.images.clear();
                }
            }
        }
    }

    // ------------------------------------------------------------- panes

    fn spawn_pane(&mut self, cwd: Option<PathBuf>) -> Result<PaneId> {
        let id = self.next_id;
        self.next_id += 1;
        // Real size is pushed by sync_sizes once the layout knows about it.
        let pane = Pane::spawn(id, &self.cfg, cwd, 80, 24)?;
        self.slots.insert(
            id,
            Slot {
                pane,
                watcher: Watcher::new(),
                state: AgentState::Idle,
                images: vec![],
            },
        );
        Ok(id)
    }

    fn focused_cwd(&self) -> Option<PathBuf> {
        self.slots.get(&self.focus()).and_then(|s| s.pane.cwd())
    }

    /// Split the focused pane. `None` when there was no room, which a key
    /// press notes and a script reports as an error.
    fn split(&mut self, dir: Dir) -> Result<Option<PaneId>> {
        let cwd = self.focused_cwd();
        let near = self.focus();
        let id = self.spawn_pane(cwd)?;
        let t = self.tab_mut();
        t.layout.set_zoom(None);
        if !t.layout.insert(id, Some(near), Some(dir)) {
            // The pty is already running, so a refused split has to kill it or
            // it lives on in `slots` with no tile: invisible and unkillable.
            self.close_pane(id);
            self.note("no room to split");
            return Ok(None);
        }
        t.focus = id;
        self.sync_sizes();
        Ok(Some(id))
    }

    fn close_pane(&mut self, id: PaneId) {
        if let Some(mut slot) = self.slots.remove(&id) {
            slot.pane.kill();
        }
        // `slots` is global but layouts are per tab: a pane that exits in a
        // background tab must be removed from *that* tab's layout, or the tab
        // keeps an id with no process and can never empty out.
        let Some(ti) = self.tabs.iter().position(|t| t.layout.ids().contains(&id)) else {
            return;
        };
        let t = &mut self.tabs[ti];
        let next = t.layout.next(id).filter(|n| *n != id);
        t.layout.remove(id);
        if t.focus == id {
            t.focus = next
                .or_else(|| t.layout.ids().first().copied())
                .unwrap_or(0);
        }
        if t.layout.zoomed == Some(id) {
            t.layout.set_zoom(None);
        }
        if t.layout.is_empty() {
            self.close_tab_at(ti);
        } else {
            self.sync_sizes();
        }
    }

    /// Reap panes whose child exited.
    fn reap(&mut self) {
        let dead: Vec<PaneId> = self
            .slots
            .iter()
            .filter(|(_, s)| s.pane.is_dead())
            .map(|(id, _)| *id)
            .collect();
        for id in dead {
            self.close_pane(id);
        }
    }

    // -------------------------------------------------------------- tabs

    fn new_tab(&mut self) -> Result<()> {
        let cwd = if self.tabs.is_empty() {
            None
        } else {
            self.focused_cwd()
        };
        let id = self.spawn_pane(cwd)?;
        let mut layout = Layout::new(self.body());
        if self.cfg.general.free_mode {
            layout.set_mode(Mode::Free);
        }
        layout.insert(id, None, None);
        self.tabs.push(Tab {
            name: format!("{}", self.tabs.len() + 1),
            layout,
            focus: id,
            renamed: false,
        });
        self.tab = self.tabs.len() - 1;
        self.sync_sizes();
        Ok(())
    }

    fn close_tab(&mut self) {
        self.close_tab_at(self.tab);
    }

    fn close_tab_at(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        for id in self.tabs[i].layout.ids() {
            if let Some(mut slot) = self.slots.remove(&id) {
                slot.pane.kill();
            }
        }
        self.tabs.remove(i);
        // Every index past `i` has shifted, so the remembered one now points
        // at a different tab. Forget it rather than jump somewhere arbitrary.
        self.last_tab = None;
        if self.tabs.is_empty() {
            self.quit = true;
            return;
        }
        if self.tab >= i {
            self.tab = self.tab.saturating_sub(1).min(self.tabs.len() - 1);
        }
        self.relayout();
    }

    fn select_tab(&mut self, i: usize) {
        if i < self.tabs.len() && i != self.tab {
            self.last_tab = Some(self.tab);
            self.tab = i;
            self.relayout();
        }
    }

    fn relayout(&mut self) {
        let body = self.body();
        for t in &mut self.tabs {
            t.layout.set_area(body);
        }
        self.sync_sizes();
    }

    // ---------------------------------------------------------- dispatch

    fn note(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now()));
    }

    /// Run one scripted command, from `ttmux send-keys` and friends. The
    /// string is what the caller prints: empty for a command that only acts.
    /// A missing pane target means the focused pane.
    pub fn script(&mut self, cmd: Cmd) -> Result<String> {
        self.script_from(cmd, None)
    }

    /// The same, run from inside `caller`: a command with no `-t` acts on
    /// that pane, as tmux reads `$TMUX_PANE`, so a script is not at the mercy
    /// of where the user has clicked since. A caller that has since closed
    /// falls back to the focused pane.
    pub fn script_from(&mut self, cmd: Cmd, caller: Option<PaneId>) -> Result<String> {
        self.caller = caller.filter(|id| self.slots.contains_key(id));
        let out = self.script_inner(cmd);
        self.caller = None;
        out
    }

    fn script_inner(&mut self, cmd: Cmd) -> Result<String> {
        match cmd {
            Cmd::Run(action) => {
                self.dispatch(action)?;
                Ok(String::new())
            }
            Cmd::SendKeys { target, keys } => {
                let id = self.pane_or_focus(target)?;
                for ev in keys {
                    let Some(s) = self.slots.get_mut(&id) else {
                        break;
                    };
                    // Same path a keystroke takes, so application cursor mode
                    // and the jump back to the live view both still apply.
                    s.pane.scroll_to_bottom();
                    let app_cursor = s.pane.screen().application_cursor();
                    let bytes = encode_key(ev, app_cursor);
                    if !bytes.is_empty() {
                        s.pane.send(&bytes);
                    }
                }
                Ok(String::new())
            }
            Cmd::CapturePane { target, history } => {
                let id = self.pane_or_focus(target)?;
                let Some(s) = self.slots.get_mut(&id) else {
                    bail!("no pane %{id}");
                };
                let text = s.pane.dump(history.is_some());
                // `-S -N`: only the last N lines of scrollback above the
                // screen, so a poll for new output is not the whole history.
                Ok(match history {
                    Some(Some(n)) => {
                        let rows = usize::from(s.pane.rows);
                        let lines: Vec<&str> = text.lines().collect();
                        let keep = lines.len().min(rows + n);
                        lines[lines.len() - keep..].join("\n")
                    }
                    _ => text,
                })
            }
            Cmd::Split { target, dir } => {
                let id = self.pane_or_focus(target)?;
                let tab = self.tab_of(id)?;
                self.select_tab(tab);
                self.set_focus(id);
                // The new pane's id, so a script can target it without a
                // trip through list-panes.
                match self.split(dir)? {
                    Some(new) => Ok(format!("%{new}")),
                    None => bail!("no room to split %{id}"),
                }
            }
            Cmd::SelectPane(id) => {
                let tab = self.tab_of(id)?;
                self.select_tab(tab);
                self.set_focus(id);
                Ok(String::new())
            }
            Cmd::ZoomPane { target } => {
                // tmux zooms the pane you name, so the focus follows it: a
                // zoomed pane nobody is typing into is not what was asked for.
                let id = self.pane_or_focus(target)?;
                let tab = self.tab_of(id)?;
                self.select_tab(tab);
                self.set_focus(id);
                self.dispatch(Action::ToggleZoom)?;
                Ok(String::new())
            }
            Cmd::ResizePane { target, dir, n } => {
                let id = self.pane_or_focus(target)?;
                let tab = self.tab_of(id)?;
                self.tabs[tab].layout.resize(id, dir, n);
                self.sync_sizes();
                Ok(String::new())
            }
            Cmd::SwapPane { src, dst } => {
                let a = self.pane_or_focus(src)?;
                let b = self.pane_or_focus(Some(dst))?;
                let ta = self.tab_of(a)?;
                let tb = self.tab_of(b)?;
                if ta != tb {
                    bail!("%{a} and %{b} are in different windows; use join-pane");
                }
                if !self.tabs[ta].layout.swap(a, b) {
                    bail!("cannot swap %{a} with %{b}");
                }
                self.sync_sizes();
                Ok(String::new())
            }
            Cmd::JoinPane {
                src,
                window,
                horizontal,
            } => {
                let id = self.pane_or_focus(src)?;
                let to = match window {
                    Some(n) => self.window_index(n)?,
                    None => self.tab,
                };
                self.move_pane_to_tab(id, to, horizontal)?;
                Ok(String::new())
            }
            Cmd::BreakPane(target) => {
                let id = self.pane_or_focus(target)?;
                self.break_pane(id)?;
                Ok(format!("{}", self.tab + 1))
            }
            Cmd::RenamePane { target, name } => {
                let id = self.pane_or_focus(target)?;
                if let Some(s) = self.slots.get_mut(&id) {
                    s.pane.title_override = Some(name);
                }
                Ok(String::new())
            }
            Cmd::KillPane(target) => {
                let id = self.pane_or_focus(target)?;
                self.close_pane(id);
                Ok(String::new())
            }
            Cmd::ListPanes { all, json } => Ok(self.list_panes(all, json)),
            Cmd::NewWindow { name } => {
                self.new_tab()?;
                let i = self.tab;
                if let Some(name) = name {
                    self.tabs[i].name = name;
                    self.tabs[i].renamed = true;
                }
                Ok(format!("{}", i + 1))
            }
            Cmd::SelectWindow(n) => {
                let i = self.window_index(n)?;
                self.select_tab(i);
                Ok(String::new())
            }
            Cmd::RenameWindow { target, name } => {
                let i = match target {
                    Some(n) => self.window_index(n)?,
                    None => self.tab,
                };
                self.tabs[i].name = name;
                self.tabs[i].renamed = true;
                Ok(String::new())
            }
            Cmd::SwapWindow { src, dst } => {
                let a = match src {
                    Some(n) => self.window_index(n)?,
                    None => self.tab,
                };
                let b = self.window_index(dst)?;
                self.tabs.swap(a, b);
                // The user is still looking at the same panes, so follow the
                // tab they were on rather than the number it used to have.
                self.tab = match self.tab {
                    t if t == a => b,
                    t if t == b => a,
                    t => t,
                };
                self.last_tab = None;
                self.relayout();
                Ok(String::new())
            }
            Cmd::MoveWindow { src, dst } => {
                let from = match src {
                    Some(n) => self.window_index(n)?,
                    None => self.tab,
                };
                let to = self.window_index(dst)?;
                let t = self.tabs.remove(from);
                self.tabs.insert(to, t);
                self.tab = match self.tab {
                    t if t == from => to,
                    t => {
                        // Everything between the two ends slides one place.
                        let mut t = t;
                        if from < t {
                            t -= 1;
                        }
                        if to <= t {
                            t += 1;
                        }
                        t
                    }
                };
                self.last_tab = None;
                self.relayout();
                Ok(String::new())
            }
            Cmd::KillWindow(target) => {
                let i = match target {
                    Some(n) => self.window_index(n)?,
                    None => self.tab,
                };
                self.close_tab_at(i);
                Ok(String::new())
            }
            Cmd::ListWindows { json } => Ok(self.list_windows(json)),
            Cmd::ListSessions { json } => Ok(self.list_sessions(json)),
            Cmd::Display(text) => {
                self.note(text);
                Ok(String::new())
            }
            Cmd::ShowOptions { key, json } => self.show_options(key.as_deref(), json),
            Cmd::SetOption { key, value } => {
                self.set_option(&key, &value)?;
                Ok(String::new())
            }
            Cmd::ListKeys { json } => Ok(self.list_keys(json)),
            Cmd::ListCommands { json } => Ok(if json {
                crate::script::commands_json()
            } else {
                crate::script::usage()
            }),
        }
    }

    // ------------------------------------------------- scripted listings

    fn list_panes(&self, all: bool, json: bool) -> String {
        let tabs: Vec<usize> = if all {
            (0..self.tabs.len()).collect()
        } else {
            vec![self.tab]
        };
        let mut rows = vec![];
        for ti in tabs {
            let t = &self.tabs[ti];
            for (id, rect) in t.layout.geometry() {
                let title = self
                    .slots
                    .get(&id)
                    .map(|s| s.pane.title())
                    .unwrap_or_default();
                rows.push((ti + 1, id, rect, title, t.focus == id && ti == self.tab));
            }
        }
        if json {
            let panes: Vec<serde_json::Value> = rows
                .iter()
                .map(|(w, id, r, title, active)| {
                    serde_json::json!({
                        "id": format!("%{id}"),
                        "window": w,
                        "width": r.w,
                        "height": r.h,
                        "title": title,
                        "active": active,
                    })
                })
                .collect();
            return json_line(&serde_json::json!({ "panes": panes }));
        }
        let mut out = String::new();
        for (w, id, r, title, active) in rows {
            let active = if active { " (active)" } else { "" };
            let win = if all { format!("{w}.") } else { String::new() };
            let _ = writeln!(out, "{win}%{id}: [{}x{}] {title}{active}", r.w, r.h);
        }
        out
    }

    fn list_windows(&self, json: bool) -> String {
        if json {
            let windows: Vec<serde_json::Value> = self
                .tabs
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    serde_json::json!({
                        "index": i + 1,
                        "name": self.tab_label(i, t),
                        "panes": t.layout.ids().len(),
                        "active": i == self.tab,
                        "zoomed": t.layout.zoomed.is_some(),
                    })
                })
                .collect();
            return json_line(&serde_json::json!({ "windows": windows }));
        }
        let mut out = String::new();
        for (i, t) in self.tabs.iter().enumerate() {
            let panes = t.layout.ids().len();
            let active = if i == self.tab { " (active)" } else { "" };
            let _ = writeln!(
                out,
                "{}: {} ({panes} panes){active}",
                i + 1,
                self.tab_label(i, t)
            );
        }
        out
    }

    fn list_sessions(&self, json: bool) -> String {
        crate::proto::sessions_report(json, Some(&self.session))
    }

    fn list_keys(&self, json: bool) -> String {
        let (map, _errors) = self.cfg.keymap();
        if json {
            let keys: Vec<serde_json::Value> = map
                .iter()
                .map(|(b, a)| serde_json::json!({"key": b.to_string(), "action": a.to_string()}))
                .collect();
            return json_line(&serde_json::json!({ "keys": keys }));
        }
        let mut out = String::new();
        for (b, a) in &map {
            let _ = writeln!(out, "{b:<16} {a}");
        }
        out
    }

    /// The config as TOML, or one dotted key of it. Reading the live config
    /// rather than the file, so it answers for what is running.
    fn show_options(&self, key: Option<&str>, json: bool) -> Result<String> {
        let all = toml::Value::try_from(&self.cfg)?;
        let Some(key) = key else {
            return Ok(if json {
                json_line(&serde_json::to_value(&self.cfg)?)
            } else {
                toml::to_string_pretty(&self.cfg)?
            });
        };
        // A binding is one name however many dots it has, the same way
        // `set_option` writes it.
        let path: Vec<&str> = match key.strip_prefix("keys.") {
            Some(chord) => vec!["keys", chord],
            None => key.split('.').collect(),
        };
        let mut cur = &all;
        for part in path {
            cur = cur
                .get(part)
                .ok_or_else(|| anyhow::anyhow!("no such option: {key}"))?;
        }
        Ok(match cur {
            _ if json => json_line(&serde_json::to_value(cur)?),
            toml::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    }

    /// Write one dotted key into the config file. The file is the config, so
    /// a scripted change survives a restart, and the hot reload applies it.
    fn set_option(&mut self, key: &str, value: &str) -> Result<()> {
        let text = std::fs::read_to_string(&self.cfg_path).unwrap_or_default();
        let mut doc: toml::Value = if text.trim().is_empty() {
            toml::Value::try_from(Config::default())?
        } else {
            toml::from_str(&text)?
        };
        // A binding is one name however many dots it has: `keys.ctrl+b .`
        // is the chord `ctrl+b .`, not a path three levels deep.
        let binding = key.strip_prefix("keys.");
        let (last, path) = match binding {
            Some(chord) => (chord, vec!["keys"]),
            None => {
                let mut parts: Vec<&str> = key.split('.').collect();
                let last = parts.pop().expect("split never yields nothing");
                (last, parts)
            }
        };
        let mut cur = &mut doc;
        for part in path {
            // A hand-written config need not have every section in it, and
            // `[keys]` is missing from most of them.
            cur = cur
                .as_table_mut()
                .filter(|t| t.contains_key(part) || part == "keys")
                .and_then(|t| {
                    t.entry(part)
                        .or_insert_with(|| toml::Value::Table(Default::default()));
                    t.get_mut(part)
                })
                .ok_or_else(|| anyhow::anyhow!("no such option: {key}"))?;
        }
        let table = cur
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("no such option: {key}"))?;
        // `keys` takes any name the user invents; everything else must
        // already be there, so a typo is an error and not a dead setting.
        if !table.contains_key(last) && binding.is_none() {
            bail!("no such option: {key}");
        }
        table.insert(last.to_string(), parse_scalar(value));
        let text = toml::to_string_pretty(&doc)?;
        // Parsed before it is written: a bad value must not leave a config
        // on disk that ttmux cannot start from.
        let cfg: Config = toml::from_str(&text)?;
        crate::config::write_private(&self.cfg_path, &text)?;
        self.apply_config(cfg);
        Ok(())
    }

    /// Move `id` into another tab's layout, splitting that tab's focused pane.
    fn move_pane_to_tab(&mut self, id: PaneId, to: usize, horizontal: bool) -> Result<()> {
        let from = self.tab_of(id)?;
        if from == to {
            bail!("%{id} is already in window {}", to + 1);
        }
        let near = self.tabs[to].focus;
        let dir = if horizontal { Dir::Right } else { Dir::Down };
        if !self.tabs[to].layout.insert(id, Some(near), Some(dir)) {
            bail!("no room in window {}", to + 1);
        }
        self.detach_pane(from, id);
        // `detach_pane` can close the tab it emptied, which shifts every
        // index after it, so the destination is found again by its contents.
        let to = self.tab_of(id)?;
        // Follow the pane, the way tmux's join-pane selects the destination.
        // `close_tab_at` may also have moved `self.tab` off the source.
        self.tab = to;
        self.tabs[to].focus = id;
        // A zoom there would hide the pane that just arrived, and typing
        // would go to a pane nothing is drawing.
        self.tabs[to].layout.set_zoom(None);
        self.relayout();
        Ok(())
    }

    /// Move `id` out into a window of its own.
    fn break_pane(&mut self, id: PaneId) -> Result<()> {
        let from = self.tab_of(id)?;
        if self.tabs[from].layout.ids().len() == 1 {
            bail!("%{id} is the only pane in its window");
        }
        let mut layout = Layout::new(self.body());
        if self.cfg.general.free_mode {
            layout.set_mode(Mode::Free);
        }
        layout.insert(id, None, None);
        self.tabs.push(Tab {
            name: format!("{}", self.tabs.len() + 1),
            layout,
            focus: id,
            renamed: false,
        });
        self.detach_pane(from, id);
        // `detach_pane` can close the tab it emptied, which shifts every
        // index after it, so the new tab is found by its contents.
        self.tab = self
            .tabs
            .iter()
            .position(|t| t.layout.ids() == [id])
            .unwrap_or(self.tabs.len() - 1);
        self.last_tab = None;
        self.relayout();
        Ok(())
    }

    /// Take a pane out of one tab's layout without killing it. The pane keeps
    /// running, which is what makes moving it possible at all.
    fn detach_pane(&mut self, tab: usize, id: PaneId) {
        let t = &mut self.tabs[tab];
        let next = t.layout.next(id).filter(|n| *n != id);
        t.layout.remove(id);
        if t.focus == id {
            t.focus = next
                .or_else(|| t.layout.ids().first().copied())
                .unwrap_or(0);
        }
        if t.layout.is_empty() {
            self.close_tab_at(tab);
        }
    }

    /// A named pane, or the focused one. A script that names a pane that has
    /// exited must hear about it rather than typing into another one.
    fn pane_or_focus(&self, target: Option<PaneId>) -> Result<PaneId> {
        match target.or(self.caller) {
            None => Ok(self.focus()),
            Some(id) if self.slots.contains_key(&id) => Ok(id),
            Some(id) => bail!("no pane %{id}"),
        }
    }

    fn tab_of(&self, id: PaneId) -> Result<usize> {
        self.tabs
            .iter()
            .position(|t| t.layout.ids().contains(&id))
            .ok_or_else(|| anyhow::anyhow!("no pane %{id}"))
    }

    /// Windows are numbered from 1 on the status bar, so a script counts
    /// them the way the screen does.
    fn window_index(&self, n: usize) -> Result<usize> {
        match n.checked_sub(1) {
            Some(i) if i < self.tabs.len() => Ok(i),
            _ => bail!("no window {n}"),
        }
    }

    fn dispatch(&mut self, action: Action) -> Result<()> {
        use Action::*;
        match action {
            Split(d) => {
                self.split(d)?;
            }
            ClosePane => {
                let id = self.focus();
                self.close_pane(id);
            }
            Focus(d) => {
                let id = self.focus();
                if let Some(n) = self.tabs[self.tab].layout.neighbor(id, d) {
                    self.set_focus(n);
                }
            }
            FocusNext | FocusPrev => {
                let id = self.focus();
                let t = self.tab_mut();
                let n = if action == FocusNext {
                    t.layout.next(id)
                } else {
                    t.layout.prev(id)
                };
                if let Some(n) = n {
                    self.set_focus(n);
                }
            }
            Resize(d, n) => {
                let id = self.focus();
                self.tab_mut().layout.resize(id, d, n);
                self.sync_sizes();
            }
            MovePane(d, n) => {
                let id = self.focus();
                self.tab_mut().layout.move_pane(id, d, n);
                self.sync_sizes();
            }
            SwapNext => {
                let id = self.focus();
                self.tab_mut().layout.swap_next(id);
                self.sync_sizes();
            }
            BreakPane => {
                let id = self.focus();
                if let Err(e) = self.break_pane(id) {
                    self.note(format!("{e:#}"));
                }
            }
            JoinPane => {
                if self.tabs.len() < 2 {
                    self.note("only one tab");
                } else {
                    self.overlay = Overlay::Prompt {
                        label: format!("Join pane into tab (1-{})", self.tabs.len()),
                        input: LineEdit::default(),
                        target: Rename::JoinTo(self.focus()),
                    }
                }
            }
            ToggleLayoutMode => {
                let t = self.tab_mut();
                let m = match t.layout.mode {
                    Mode::Tiling => Mode::Free,
                    Mode::Free => Mode::Tiling,
                };
                t.layout.set_mode(m);
                self.sync_sizes();
                let label = match m {
                    Mode::Tiling => "tiling",
                    Mode::Free => "free",
                };
                self.note(format!("{label} mode"));
            }
            SetPreset(p) => {
                self.tab_mut().layout.set_preset(p);
                self.sync_sizes();
                self.note(p.as_str());
            }
            NextPreset => {
                let t = self.tab_mut();
                let p = match t.layout.preset {
                    Preset::EvenHorizontal => Preset::EvenVertical,
                    Preset::EvenVertical => Preset::MainVertical,
                    Preset::MainVertical => Preset::MainHorizontal,
                    Preset::MainHorizontal | Preset::Tree => Preset::EvenHorizontal,
                };
                t.layout.set_preset(p);
                self.sync_sizes();
                self.note(format!("{p:?}"));
            }
            ToggleZoom => {
                let id = self.focus();
                let t = self.tab_mut();
                let z = if t.layout.zoomed == Some(id) {
                    None
                } else {
                    Some(id)
                };
                t.layout.set_zoom(z);
                self.sync_sizes();
            }
            ToggleFloat => {
                let id = self.focus();
                self.tab_mut().layout.toggle_float(id);
                self.sync_sizes();
            }
            NewTab => self.new_tab()?,
            CloseTab => self.close_tab(),
            NextTab => {
                let n = (self.tab + 1) % self.tabs.len();
                self.select_tab(n);
            }
            PrevTab => {
                let n = (self.tab + self.tabs.len() - 1) % self.tabs.len();
                self.select_tab(n);
            }
            SelectTab(i) => self.select_tab(i.saturating_sub(1)),
            LastTab => {
                if let Some(i) = self.last_tab {
                    self.select_tab(i);
                }
            }
            RenameTab => {
                self.overlay = Overlay::Prompt {
                    label: "Rename tab".into(),
                    input: LineEdit::new(self.tabs[self.tab].name.clone()),
                    target: Rename::Tab,
                }
            }
            MoveTab(d) => {
                let to = match d {
                    Dir::Left | Dir::Up => self.tab.checked_sub(1),
                    Dir::Right | Dir::Down => {
                        (self.tab + 1 < self.tabs.len()).then_some(self.tab + 1)
                    }
                };
                if let Some(to) = to {
                    self.tabs.swap(self.tab, to);
                    // `last_tab` names a position, and both positions just
                    // changed what they hold.
                    self.last_tab = None;
                    self.tab = to;
                }
            }
            RenamePane => {
                let id = self.focus();
                let now = self
                    .slots
                    .get(&id)
                    .map(|s| s.pane.title())
                    .unwrap_or_default();
                self.overlay = Overlay::Prompt {
                    label: "Rename pane".into(),
                    input: LineEdit::new(now),
                    target: Rename::Pane(id),
                }
            }
            ScrollUp(n) => self.scroll(-(n as isize)),
            ScrollDown(n) => self.scroll(n as isize),
            ScrollTop => self.scroll(-1_000_000),
            ScrollBottom => {
                let id = self.focus();
                if let Some(s) = self.slots.get_mut(&id) {
                    s.pane.scroll_to_bottom();
                }
            }
            ToggleSettings => {
                self.overlay = match self.overlay {
                    Overlay::Settings(_) => Overlay::None,
                    _ => Overlay::Settings(Settings::new()),
                }
            }
            ToggleHelp => {
                self.overlay = match self.overlay {
                    Overlay::Help { .. } => Overlay::None,
                    _ => Overlay::Help { scroll: 0 },
                }
            }
            CommandPalette => {
                self.overlay = Overlay::Palette {
                    query: LineEdit::default(),
                    sel: 0,
                }
            }
            CommandLine => self.overlay = Overlay::Command(CmdLine::new(self.cmd_history.clone())),
            NextAlert => match self.next_alert() {
                Some((tab, id)) => {
                    self.tab = tab;
                    self.set_focus(id);
                    self.relayout();
                }
                None => self.note("no panes waiting"),
            },
            ReloadConfig => match Config::load(&self.cfg_path.clone()) {
                Ok(c) => {
                    self.apply_config(c);
                    self.note("config reloaded");
                }
                Err(e) => self.note(format!("config: {e}")),
            },
            SendPrefix => {
                if let Some(c) = self.cfg.prefixes().first() {
                    self.type_into_pane(KeyEvent::new(c.code, c.mods));
                }
            }
            Quit => self.quit = true,
            Detach => self.detached = true,
            Nop => {}
        }
        Ok(())
    }

    fn apply_config(&mut self, cfg: Config) {
        // Mouse capture is a terminal mode, not a flag we can read back later:
        // toggling `general.mouse` has to actually turn it on or off now.
        if cfg.general.mouse != self.cfg.general.mouse {
            let mut out = io::stdout();
            let _ = if cfg.general.mouse {
                execute!(out, event::EnableMouseCapture)
            } else {
                execute!(out, event::DisableMouseCapture)
            };
        }
        // Agent states are only refreshed while agents are enabled, so clear
        // them here or the last alert stays lit forever.
        if !cfg.agents.enabled {
            for slot in self.slots.values_mut() {
                slot.state = AgentState::Idle;
                slot.watcher = Watcher::default();
            }
        }
        self.cfg = cfg;
        self.keys.reload(&self.cfg);
        self.widgets.reload(&self.cfg.status.widgets);
        self.cfg_text = std::fs::read_to_string(&self.cfg_path).unwrap_or_default();
        self.relayout();
    }

    /// What every named custom widget last printed.
    ///
    /// Only the names a row actually lists are collected: a widget defined
    /// but not placed still runs, and this keeps it out of the frame.
    fn widget_output(&self) -> BTreeMap<String, String> {
        self.cfg
            .status
            .widgets
            .keys()
            .filter_map(|n| self.widgets.output(n).map(|out| (n.clone(), out)))
            .collect()
    }

    /// Reload the config if something else wrote the file.
    ///
    /// The text is what is compared, not the mtime: the file is small, and a
    /// stamp misses an edit that lands in the same clock tick as the read.
    /// Saving from the settings UI goes through `apply_config`, which records
    /// the new text, so this only fires for an edit made outside ttmux.
    fn reload_if_changed(&mut self) {
        let now = std::fs::read_to_string(&self.cfg_path).unwrap_or_default();
        if now == self.cfg_text {
            return;
        }
        // Recorded either way: a config that does not parse must not be
        // retried every tick, filling the bar with the same error.
        self.cfg_text = now.clone();
        match toml::from_str::<Config>(&now) {
            Ok(c) => {
                self.apply_config(c);
                self.note("config reloaded");
            }
            Err(e) => self.note(format!("config: {e}")),
        }
    }

    fn scroll(&mut self, delta: isize) {
        let id = self.focus();
        if let Some(s) = self.slots.get_mut(&id) {
            s.pane.scroll_by(delta);
        }
    }

    /// The next pane, in tab then layout order, that wants attention.
    fn next_alert(&self) -> Option<(usize, PaneId)> {
        let order: Vec<(usize, PaneId)> = self
            .tabs
            .iter()
            .enumerate()
            .flat_map(|(ti, t)| t.layout.ids().into_iter().map(move |id| (ti, id)))
            .collect();
        let here = order
            .iter()
            .position(|(ti, id)| *ti == self.tab && *id == self.focus())
            .unwrap_or(0);
        (1..=order.len())
            .map(|k| order[(here + k) % order.len()])
            .find(|(_, id)| self.slots.get(id).is_some_and(|s| s.state.is_alert()))
    }

    // ------------------------------------------------------------- input

    /// True if the frame has to be repainted. A key that goes straight to a
    /// pane changes nothing ttmux draws; the program's echo does, and
    /// repainting for both is a whole wasted frame per keystroke.
    fn on_key(&mut self, ev: KeyEvent) -> Result<bool> {
        if ev.kind == KeyEventKind::Release {
            return Ok(false);
        }
        // Overlays swallow keys first.
        match &mut self.overlay {
            Overlay::Help { scroll } => {
                let max = help_max_scroll(&self.cfg, self.area);
                match ev.code {
                    KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => *scroll = (*scroll + 1).min(max),
                    KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                    KeyCode::PageDown => *scroll = (*scroll + 10).min(max),
                    _ => self.overlay = Overlay::None,
                }
                return Ok(true);
            }
            Overlay::Settings(s) => {
                let out = s.on_key(ev, &mut self.cfg);
                return self.after_settings(out).map(|_| true);
            }
            Overlay::Palette { .. } => return self.palette_key(ev).map(|_| true),
            Overlay::Prompt { .. } => return self.prompt_key(ev).map(|_| true),
            Overlay::Command(c) => {
                let out = c.key(ev);
                return self.after_command_line(out).map(|_| true);
            }
            Overlay::Welcome(w) => {
                let out = w.on_key(ev, &mut self.cfg);
                return self.after_welcome(out).map(|_| true);
            }
            Overlay::None => {}
        }

        match self.keys.resolve(ev) {
            Resolution::Action(a) => self.dispatch(a).map(|_| true),
            // The status bar shows that a prefix is held.
            Resolution::Pending => Ok(true),
            Resolution::Passthrough => Ok(self.type_into_pane(ev)),
        }
    }

    /// Write a key to the focused pane as a terminal would. True if the view
    /// moved, which only happens when the pane was scrolled back.
    fn type_into_pane(&mut self, ev: KeyEvent) -> bool {
        let id = self.focus();
        let Some(s) = self.slots.get_mut(&id) else {
            return false;
        };
        // Typing anywhere jumps back to the live view, like a real terminal.
        let scrolled = s.pane.scroll != 0;
        s.pane.scroll_to_bottom();
        let app_cursor = s.pane.screen().application_cursor();
        let bytes = encode_key(ev, app_cursor);
        if !bytes.is_empty() {
            s.pane.send(&bytes);
        }
        scrolled
    }

    fn after_settings(&mut self, out: Outcome) -> Result<()> {
        match out {
            Outcome::Continue => {}
            Outcome::Close => self.overlay = Overlay::None,
            Outcome::Apply => {
                let cfg = self.cfg.clone();
                self.apply_config(cfg);
            }
            Outcome::Save => {
                let cfg = self.cfg.clone();
                self.apply_config(cfg);
                match self.cfg.save(&self.cfg_path) {
                    Ok(()) => self.note(format!("saved {}", self.cfg_path.display())),
                    Err(e) => self.note(format!("save failed: {e}")),
                }
            }
        }
        Ok(())
    }

    /// The picker writes the config itself: the file's absence is what put
    /// the overlay on screen, so creating it is how the choice sticks.
    fn after_welcome(&mut self, out: crate::onboarding::Outcome) -> Result<()> {
        use crate::onboarding::Outcome as W;
        match out {
            W::Continue => {}
            W::Done => self.overlay = Overlay::None,
            W::Save => {
                let cfg = self.cfg.clone();
                self.apply_config(cfg);
                if let Err(e) = self.cfg.save(&self.cfg_path) {
                    self.note(format!("save failed: {e}"));
                }
            }
        }
        Ok(())
    }

    fn palette_matches(query: &str) -> Vec<&'static Action> {
        let q = query.to_ascii_lowercase();
        ALL_ACTIONS
            .iter()
            .filter(|a| a.to_string().to_ascii_lowercase().contains(&q))
            .collect()
    }

    fn palette_key(&mut self, ev: KeyEvent) -> Result<()> {
        let Overlay::Palette { query, sel } = &mut self.overlay else {
            return Ok(());
        };
        // The field gets first refusal; it leaves Enter, Esc and the arrows.
        if query.key(ev) {
            *sel = 0;
            return Ok(());
        }
        match ev.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Up => *sel = sel.saturating_sub(1),
            KeyCode::Down => *sel += 1,
            KeyCode::Enter => {
                let hits = Self::palette_matches(query.text());
                let action = hits.get((*sel).min(hits.len().saturating_sub(1))).copied();
                self.overlay = Overlay::None;
                if let Some(a) = action {
                    self.dispatch(a.clone())?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn prompt_key(&mut self, ev: KeyEvent) -> Result<()> {
        let Overlay::Prompt { input, target, .. } = &mut self.overlay else {
            return Ok(());
        };
        if input.key(ev) {
            return Ok(());
        }
        match ev.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Enter => {
                let name = input.text().to_string();
                let target = *target;
                self.overlay = Overlay::None;
                if name.is_empty() {
                    return Ok(());
                }
                match target {
                    Rename::Tab => {
                        let t = self.tab_mut();
                        t.name = name;
                        // Sticky: an escape sequence from a program must not
                        // take a name back off the user.
                        t.renamed = true;
                    }
                    Rename::Pane(id) => {
                        if let Some(s) = self.slots.get_mut(&id) {
                            s.pane.title_override = Some(name);
                        }
                    }
                    Rename::JoinTo(id) => match name.trim().parse::<usize>() {
                        Ok(n) if n >= 1 && n <= self.tabs.len() && n - 1 != self.tab => {
                            if let Err(e) = self.move_pane_to_tab(id, n - 1, false) {
                                self.note(format!("{e:#}"));
                            }
                        }
                        _ => self.note(format!("no tab {name}")),
                    },
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// What the `:` line asked for. A command runs through the same parser
    /// and the same `script` as one typed in a shell, so the two cannot drift.
    fn after_command_line(&mut self, out: CmdOutcome) -> Result<()> {
        let line = match out {
            CmdOutcome::Stay => return Ok(()),
            CmdOutcome::Cancel => {
                self.overlay = Overlay::None;
                return Ok(());
            }
            CmdOutcome::Run(line) => line,
        };
        self.overlay = Overlay::None;
        self.cmd_history.push(line.clone());
        let words = crate::script::split(&line);
        let Some((verb, args)) = words.split_first() else {
            return Ok(());
        };
        match crate::script::parse(verb, args).and_then(|c| self.script(c)) {
            Ok(text) if text.is_empty() => {}
            Ok(text) => self.note(text.lines().next().unwrap_or_default().to_string()),
            Err(e) => self.note(format!("{e:#}")),
        }
        Ok(())
    }

    fn on_mouse(&mut self, ev: MouseEvent) -> Result<()> {
        if !self.cfg.general.mouse {
            return Ok(());
        }
        let (x, y) = (ev.column, ev.row);

        if let Overlay::Settings(s) = &mut self.overlay {
            let area = overlay_rect(self.area);
            let out = s.on_mouse(ev, area, &mut self.cfg);
            return self.after_settings(out);
        }

        // Status rows: click a tab.
        if self.status_rows().iter().any(|(r, _)| r.contains(x, y)) {
            if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
                if let Some(i) = self
                    .tab_hits
                    .iter()
                    .find(|(_, row, r)| *row == y && r.contains(&x))
                    .map(|(i, _, _)| *i)
                {
                    self.select_tab(i);
                }
            }
            return Ok(());
        }

        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(id) = self.tabs[self.tab].layout.pane_at(x, y) {
                    self.set_focus(id);
                    self.tab_mut().layout.raise(id);
                }
                if self.tabs[self.tab].layout.drag_start(x, y) {
                    self.sync_sizes();
                } else {
                    self.forward_mouse(ev);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.tabs[self.tab].layout.dragging() {
                    self.tab_mut().layout.drag_to(x, y);
                    self.sync_sizes();
                } else {
                    self.forward_mouse(ev);
                }
            }
            MouseEventKind::Up(_) => {
                if self.tabs[self.tab].layout.dragging() {
                    self.tab_mut().layout.drag_end();
                    self.sync_sizes();
                } else {
                    self.forward_mouse(ev);
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let over = self.tabs[self.tab].layout.pane_at(x, y);
                let wants = over
                    .and_then(|id| self.slots.get(&id))
                    .is_some_and(|s| wants_mouse(s.pane.screen()));
                if wants {
                    self.forward_mouse(ev);
                } else if let Some(s) = over.and_then(|id| self.slots.get_mut(&id)) {
                    let delta = if ev.kind == MouseEventKind::ScrollUp {
                        -3
                    } else {
                        3
                    };
                    s.pane.scroll_by(delta);
                }
            }
            MouseEventKind::Moved if self.cfg.general.focus_follows_mouse => {
                if let Some(id) = self.tabs[self.tab].layout.pane_at(x, y) {
                    self.set_focus(id);
                }
            }
            _ => self.forward_mouse(ev),
        }
        Ok(())
    }

    /// Send a mouse event to the pane under the pointer, if it asked for one.
    fn forward_mouse(&mut self, ev: MouseEvent) {
        let Some(id) = self.tabs[self.tab].layout.pane_at(ev.column, ev.row) else {
            return;
        };
        let Some(outer) = self.tabs[self.tab].layout.rect_of(id) else {
            return;
        };
        let inner = self.inner(id, outer);
        if !inner.contains(ev.column, ev.row) {
            return;
        }
        if let Some(s) = self.slots.get_mut(&id) {
            let sc = s.pane.screen();
            let (mode, encoding) = (sc.mouse_protocol_mode(), sc.mouse_protocol_encoding());
            if let Some(bytes) =
                encode_mouse(ev, ev.column - inner.x, ev.row - inner.y, mode, encoding)
            {
                s.pane.send(&bytes);
            }
        }
    }

    // ------------------------------------------------------------- loop

    pub fn main_loop<B: Backend>(
        &mut self,
        term: &mut Terminal<B>,
        host: &mut dyn Host,
    ) -> Result<Exit>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let mut dirty = true;
        let mut last_tick = Instant::now();
        let mut last_busy = Instant::now();
        loop {
            if let Some(e) = self.exit() {
                return Ok(e);
            }
            // A dead event source is not a crash: the panes are untouched and
            // only this view ends, which is exactly what detaching means.
            let tick = if last_busy.elapsed() < BUSY_FOR {
                TICK_BUSY
            } else {
                TICK_IDLE
            };
            let ev = match host.poll(tick) {
                Ok(ev) => ev,
                Err(_) => return Ok(Exit::Detached),
            };
            if let Some(ev) = ev {
                dirty |= match ev {
                    Event::Key(k) => self.on_key(k)?,
                    Event::Mouse(m) => {
                        self.on_mouse(m)?;
                        true
                    }
                    // Like typing: what lands on the screen is the pane's
                    // echo, and that marks the frame dirty on its own.
                    Event::Paste(text) => {
                        let id = self.focus();
                        if let Some(s) = self.slots.get_mut(&id) {
                            // Only a guest that set DECSET 2004 parses the
                            // brackets; to anything else they are literal text.
                            let bracket = s.pane.screen().bracketed_paste();
                            if bracket {
                                s.pane.send(b"\x1b[200~");
                            }
                            s.pane.send(text.as_bytes());
                            if bracket {
                                s.pane.send(b"\x1b[201~");
                            }
                        }
                        false
                    }
                    Event::Resize(w, h) => {
                        self.area = Rect::new(0, 0, w, h);
                        self.relayout();
                        true
                    }
                    _ => false,
                };
                last_busy = Instant::now();
            }

            for job in host.commands() {
                let out = self
                    .script_from(job.cmd, job.caller)
                    .map_err(|e| format!("{e:#}"));
                let _ = job.reply.try_send(out);
                dirty = true;
                last_busy = Instant::now();
            }

            let mut output = false;
            let ids: Vec<PaneId> = self.slots.keys().copied().collect();
            for id in ids {
                if let Some(s) = self.slots.get_mut(&id) {
                    if s.pane.pump() {
                        output = true;
                    }
                }
            }
            if output {
                dirty = true;
                last_busy = Instant::now();
            }
            // The clock widget and the agent busy->idle grace timer both move
            // on their own, so redraw at least once a second regardless of I/O.
            let second = last_tick.elapsed() >= Duration::from_secs(1);
            if second {
                last_tick = Instant::now();
                dirty = true;
            }
            if output || second {
                self.update_agents(host)?;
            }
            if second {
                self.reload_if_changed();
                self.widgets.tick();
            }
            self.reap();
            if let Some(e) = self.exit() {
                return Ok(e);
            }

            if self
                .message
                .as_ref()
                .is_some_and(|(_, t)| t.elapsed() > MESSAGE_TTL)
            {
                self.message = None;
                dirty = true;
            }

            if dirty {
                self.draw(term, host)?;
                dirty = false;
            }
        }
    }

    /// Whether the loop is over, and on whose terms.
    fn exit(&self) -> Option<Exit> {
        match (self.quit, self.detached) {
            (true, _) => Some(Exit::Quit),
            (_, true) => Some(Exit::Detached),
            _ => None,
        }
    }

    fn update_agents(&mut self, host: &mut dyn Host) -> Result<()> {
        if !self.cfg.agents.enabled {
            return Ok(());
        }
        let bell_on = self.cfg.agents.bell_on_attention;
        let mut ring = false;
        let cfg = self.cfg.agents.clone();
        for slot in self.slots.values_mut() {
            let title = slot.pane.title();
            let tail = slot.pane.tail(6);
            let bell = std::mem::take(&mut slot.pane.bell);
            slot.state = slot.watcher.update(&cfg, &title, &tail, bell);
            if slot.watcher.take_alert() && bell_on {
                ring = true;
            }
        }
        if ring {
            // Pass the bell through to the outer terminal so the OS notifies.
            host.passthrough(b"\x07")?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ render

    pub fn draw<B: Backend>(&mut self, term: &mut Terminal<B>, host: &mut dyn Host) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        // The backend can change size between the resize event and this
        // frame: with a server, a client's resize reaches the backend at
        // once, while `self.area` only moves when the app drains the event.
        // ratatui sizes the buffer from the backend, so drawing against a
        // stale `self.area` puts the status row outside it.
        let size = term.size()?;
        if (size.width, size.height) != (self.area.w, self.area.h) {
            self.area = Rect::new(0, 0, size.width, size.height);
            self.relayout();
        }

        // Snapshot what the closure needs; `draw` borrows `self` mutably.
        let body = self.body();
        if self.tabs.is_empty() {
            return Ok(());
        }
        let mut cursor: Option<(u16, u16)> = None;
        let mut hits = vec![];
        let mut places: Vec<(PaneId, Rect)> = vec![];

        // The cell diff and the image replay are two separate writes, so the
        // host can paint between them and tear. Both tuios and OpenTUI wrap
        // the frame in DECSET 2026 and neither probes for support first:
        // tuios records that querying is what made it unreliable, since a
        // host over SSH or Apple Terminal never answers, and an unknown DEC
        // private mode is ignored anyway.
        host.passthrough(b"\x1b[?2026h")?;
        term.draw(|f| {
            // A second client can resize between the check above and here.
            // Painting geometry that was computed for another size is what
            // indexes past the buffer, so skip the frame; the resize event
            // behind it marks the app dirty and the next one lands right.
            if f.area() != ratatui::layout::Rect::from(self.area) {
                return;
            }
            let buf = f.buffer_mut();
            buf.set_style(body.into(), Style::default());
            let focus = self.tabs[self.tab].focus;
            let zoomed = self.tabs[self.tab].layout.zoomed.is_some();

            for (id, outer) in self.tabs[self.tab].layout.geometry() {
                let Some(slot) = self.slots.get(&id) else {
                    continue;
                };
                let (framed, inner) = self.frame(id, outer);
                let floating = self.tabs[self.tab].layout.is_floating(id);
                if floating && self.cfg.appearance.float_shadow {
                    render::draw_shadow(buf, framed);
                }
                let focused = id == focus;
                // A float has no neighbours to share a line with, so it keeps
                // a box even in divider mode, or it reads as part of the pane
                // underneath it.
                if self.cfg.appearance.border_style == BorderStyle::Divider && !floating {
                    let (left, top) = self.divider_sides(framed);
                    render::draw_divider(
                        buf,
                        framed,
                        left,
                        top,
                        focused,
                        slot.state.is_alert(),
                        &self.cfg.appearance,
                    );
                } else {
                    let mut ap = self.cfg.appearance.clone();
                    if ap.border_style == BorderStyle::Divider {
                        ap.border_style = BorderStyle::Square;
                    }
                    render::draw_border(
                        buf,
                        framed,
                        &slot.pane.title(),
                        focused,
                        slot.state.is_alert(),
                        zoomed && focused,
                        &ap,
                    );
                }
                render::draw_screen(
                    buf,
                    inner,
                    slot.pane.screen(),
                    self.cfg.appearance.dim_unfocused && !focused,
                );
                places.push((id, inner));
                if focused {
                    let sc = slot.pane.screen();
                    if !sc.hide_cursor() && slot.pane.scroll == 0 {
                        let (row, col) = sc.cursor_position();
                        let (cx, cy) = (inner.x + col, inner.y + row);
                        if inner.contains(cx, cy) {
                            cursor = Some((cx, cy));
                        }
                    }
                }
            }

            if let Some((_, _, half)) = self.tabs[self.tab].layout.snap_target() {
                render::draw_snap_preview(buf, half, self.cfg.status.accent.into());
            }

            let rows = self.status_rows();
            if !rows.is_empty() {
                let tabs: Vec<(String, bool)> = self
                    .tabs
                    .iter()
                    .enumerate()
                    .map(|(i, t)| (self.tab_label(i, t), i == self.tab))
                    .collect();
                let panes: Vec<(String, AgentState)> = self.tabs[self.tab]
                    .layout
                    .ids()
                    .iter()
                    .filter_map(|id| self.slots.get(id).map(|s| (s.pane.title(), s.state)))
                    .collect();
                let alerts = self.slots.values().filter(|s| s.state.is_alert()).count();
                let custom = self.widget_output();
                let ctx = status::Ctx {
                    session: &self.session,
                    mode: match self.tabs[self.tab].layout.mode {
                        Mode::Tiling => "tiling",
                        Mode::Free => "free",
                    },
                    zoomed,
                    tabs: &tabs,
                    panes: &panes,
                    alerts,
                    message: self.message.as_ref().map(|(m, _)| m.as_str()),
                    pending_prefix: self.keys.pending(),
                    custom: &custom,
                };
                for (r, bar) in rows {
                    hits.extend(
                        status::draw(buf, r, &self.cfg.status, bar, &ctx)
                            .into_iter()
                            .map(|(i, cols)| (i, r.y, cols)),
                    );
                }
            }

            match &self.overlay {
                Overlay::None => {}
                Overlay::Help { scroll } => {
                    draw_help(buf, overlay_rect(self.area), &self.cfg, *scroll);
                    cursor = None;
                }
                Overlay::Settings(s) => {
                    s.draw(buf, overlay_rect(self.area), &self.cfg);
                    cursor = None;
                }
                Overlay::Palette { query, sel } => {
                    draw_palette(buf, overlay_rect(self.area), query, *sel, &self.cfg);
                    cursor = None;
                }
                Overlay::Welcome(w) => {
                    let rect = overlay_rect(self.area);
                    let inner = modal(buf, rect, w.title(), w.hint(), &self.cfg);
                    w.draw(buf, inner, &self.cfg);
                }
                Overlay::Prompt { label, input, .. } => {
                    draw_prompt(buf, self.area, label, input, &self.cfg);
                    cursor = None;
                }
                Overlay::Command(c) => {
                    c.draw(buf, self.area, &self.cfg);
                    cursor = None;
                }
            }

            // The which-key popup: a prefix is held, so show what can follow
            // it rather than making the binding a memory test.
            if self.cfg.general.which_key {
                if let Some(prefix) = self.keys.pending_prefix() {
                    crate::whichkey::draw(buf, self.area, prefix, &self.cfg);
                }
            }

            // An overlay owns the screen, but graphics sit in a layer above
            // every cell, so an image would float on top of it.
            if !matches!(self.overlay, Overlay::None | Overlay::Command(_)) {
                places.clear();
            }

            if let Some((x, y)) = cursor {
                f.set_cursor_position(Position::new(x, y));
            }
        })?;
        let replayed = self.replay_images(&places, host);
        host.passthrough(b"\x1b[?2026l")?;
        replayed?;
        self.tab_hits = hits;
        Ok(())
    }

    /// Re-emit captured graphics after the frame.
    ///
    /// Terminals draw images into their own layer at the real cursor, not
    /// into ratatui's buffer, so this has to run once `term.draw` has
    /// finished painting or the next diff simply covers them.
    fn replay_images(&mut self, places: &[(PaneId, Rect)], host: &mut dyn Host) -> Result<()> {
        for slot in self.slots.values_mut() {
            let new = slot.pane.take_images();
            if !self.cfg.general.passthrough_images {
                continue;
            }
            merge_images(&mut slot.images, new);
            // Scrolling back makes the recorded cell meaningless: the image
            // belongs to a line that is no longer where it was.
            if slot.pane.scroll != 0 {
                slot.images.clear();
            }
            let cols = slot.pane.cols;
            slot.images.retain(|i| i.col < cols);
        }
        let pending = self.slots.values().any(|s| !s.images.is_empty());
        if !pending && !self.drew_graphics {
            return Ok(());
        }
        // Kitty keeps a placement until told to drop it; iTerm2 and sixel
        // images are cell content, which the repaint above already erased.
        // Each placement moves the host cursor, so save it first: the
        // position `term.draw` just set has to survive the replay. From the
        // kitty placement path in tuios.
        let mut out: Vec<u8> = b"\x1b7\x1b_Ga=d\x1b\\".to_vec();
        let mut drew = false;
        for (id, inner) in places {
            let Some(slot) = self.slots.get(id) else {
                continue;
            };
            for img in &slot.images {
                let x = inner.x + img.col;
                if x >= inner.right() {
                    continue;
                }
                // tuios: an image placed past the last row makes the host
                // scroll to make room, and the next frame places at the same
                // now-scrolled cell, duplicating for ever. Clamp into the
                // pane and the screen, so the overflow is clipped by the
                // host rather than the whole image being hidden.
                let y = (inner.y + img.row)
                    .min(inner.bottom().saturating_sub(1))
                    .min(self.area.h.saturating_sub(1));
                queue!(out, MoveTo(x, y))?;
                out.write_all(&img.bytes)?;
                drew = true;
            }
        }
        out.extend_from_slice(b"\x1b8");
        host.passthrough(&out)?;
        self.drew_graphics = drew;
        Ok(())
    }

    /// A tab shows the program's own title only while it has one pane: with
    /// a split, that title belongs to a pane, and the tab keeps its number.
    /// A tab the user named keeps that name either way.
    fn tab_label(&self, i: usize, t: &Tab) -> String {
        if t.renamed {
            return t.name.clone();
        }
        let title = if t.layout.ids().len() == 1 {
            self.slots
                .get(&t.focus)
                .map(|s| s.pane.title())
                .unwrap_or_default()
        } else {
            String::new()
        };
        if title.is_empty() {
            format!("{}", i + 1)
        } else {
            format!("{}:{}", i + 1, title)
        }
    }
}

/// Does this pane want raw mouse events (vim, htop, a TUI) rather than ttmux
/// handling the click itself?
fn wants_mouse(screen: &vt100::Screen) -> bool {
    screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None
}

// ------------------------------------------------------------- overlays

fn overlay_rect(area: Rect) -> Rect {
    let w = area.w.saturating_sub(8).clamp(20, 90);
    let h = area.h.saturating_sub(4).clamp(8, 30);
    Rect::new(
        area.x + (area.w.saturating_sub(w)) / 2,
        area.y + (area.h.saturating_sub(h)) / 2,
        w.min(area.w),
        h.min(area.h),
    )
}

/// A drop shadow twice as wide as it is deep.
///
/// Terminal cells are about twice as tall as they are wide, so a one-cell L
/// reads lopsided. The second pass is the same L one column further out;
/// the union is a two-column right edge and a one-row bottom, all of it
/// outside `rect`.
fn shadow(buf: &mut Buffer, rect: Rect) {
    render::draw_shadow(buf, rect);
    render::draw_shadow(
        buf,
        Rect {
            w: rect.w.saturating_add(1),
            ..rect
        },
    );
}

/// The modal ground: a few steps off the bar background, so a modal reads as
/// lifted off the panes rather than as one more window.
///
/// Dark themes go up and light ones go down, which keeps the contrast with
/// `status.fg` in the direction it already had. A palette colour has no
/// arithmetic to do and leans on the border and the shadow instead.
///
/// Public because anything drawing inside a modal has to paint on the same
/// ground: [`crate::settings_ui::put`] resets each cell it writes.
pub fn modal_bg(cfg: &Config) -> Color {
    const STEP: u8 = 20;
    match cfg.status.bg.into() {
        Color::Rgb(r, g, b) => {
            let up = u16::from(r) + u16::from(g) + u16::from(b) < 384;
            let f = |v: u8| {
                if up {
                    v.saturating_add(STEP)
                } else {
                    v.saturating_sub(STEP)
                }
            };
            Color::Rgb(f(r), f(g), f(b))
        }
        other => other,
    }
}

/// The content rect inside the modal chrome, hint row already taken out.
///
/// `modal` and `Settings::on_mouse` both need it, so where a click lands and
/// where a row was drawn cannot drift apart.
pub fn modal_content(rect: Rect) -> Rect {
    let mut inner = modal_box(rect);
    // The hint is the first thing to go: content outranks it.
    if inner.h > 1 {
        inner.h -= 1;
    }
    inner
}

/// Everything inside the border, hint row included.
///
/// Text flush against the border is hard to read, so this is inset one cell
/// at the sides and one row under the title chip.
fn modal_box(rect: Rect) -> Rect {
    let mut b = rect.shrink(1);
    if b.w > 2 {
        b.x += 1;
        b.w -= 2;
    }
    if b.h > 2 {
        b.y += 1;
        b.h -= 1;
    }
    b
}

/// The chrome every modal wears: shadow, raised ground, heavy accent border,
/// a title chip and a dim hint on the bottom row. Returns the content rect.
///
/// One function so help, settings, the palette, the prompt, the welcome and
/// the colour picker cannot drift into six different looks.
pub fn modal(buf: &mut Buffer, rect: Rect, title: &str, hint: &str, cfg: &Config) -> Rect {
    let inner = modal_content(rect);
    if rect.w == 0 || rect.h == 0 {
        return inner;
    }
    let fg: Color = cfg.status.fg.into();
    let bg = modal_bg(cfg);
    shadow(buf, rect);
    render::clear(buf, rect, Style::default().bg(bg).fg(fg));
    render::draw_border(
        buf,
        rect,
        "",
        true,
        false,
        false,
        // Heavy glyphs: an overlay is a different kind of thing from a pane,
        // and the doubled stroke says so even when the accent colour does not.
        &crate::config::Appearance {
            border_focused: cfg.status.accent,
            border_style: crate::config::BorderStyle::Heavy,
            ..cfg.appearance.clone()
        },
    );

    // The title as a filled chip rather than accent text on the rule: it is
    // the one thing on screen that has to be found without looking for it.
    if !title.is_empty() && rect.w > 6 {
        crate::settings_ui::put(
            buf,
            rect.x + 2,
            rect.y,
            &format!("  {title}  "),
            rect.w - 4,
            Style::default()
                .bg(cfg.status.accent.into())
                .fg(cfg.status.bg.into())
                .add_modifier(Modifier::BOLD),
        );
    }

    if !hint.is_empty() && inner.h < modal_box(rect).h {
        crate::settings_ui::put(
            buf,
            inner.x,
            inner.bottom(),
            hint,
            inner.w,
            Style::default().bg(bg).fg(fg).add_modifier(Modifier::DIM),
        );
    }
    inner
}

fn draw_help(buf: &mut Buffer, rect: Rect, cfg: &Config, scroll: usize) {
    let (map, _) = cfg.keymap();
    // The keymap is longer than any screen is tall, so the help scrolls
    // rather than quietly hiding the bindings past the bottom.
    let rows = modal_content(rect).h as usize;
    let hint = format!(
        "{}-{} of {} — up and down scroll, any other key closes.",
        (scroll + 1).min(map.len()),
        (scroll + rows).min(map.len()),
        map.len()
    );
    let inner = modal(buf, rect, "Help", &hint, cfg);
    let style = Style::default().fg(cfg.status.fg.into());
    let accent = Style::default().fg(cfg.status.accent.into());
    for (i, (binding, action)) in map.iter().skip(scroll).enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.h {
            break;
        }
        buf.set_stringn(inner.x, y, binding.to_string(), 22, accent);
        buf.set_stringn(
            inner.x + 24,
            y,
            action.to_string(),
            inner.w.saturating_sub(24) as usize,
            style,
        );
    }
}

/// How far `help` can scroll: the last page still fills the box.
fn help_max_scroll(cfg: &Config, area: Rect) -> usize {
    let rows = modal_content(overlay_rect(area)).h as usize;
    cfg.keymap().0.len().saturating_sub(rows)
}

fn draw_palette(buf: &mut Buffer, rect: Rect, query: &LineEdit, sel: usize, cfg: &Config) {
    let inner = modal(
        buf,
        rect,
        "Commands",
        "Type to filter, ↑↓ to select, Enter to run, Esc to cancel",
        cfg,
    );
    let style = Style::default().fg(cfg.status.fg.into());
    let sel_style = Style::default()
        .fg(cfg.status.bg.into())
        .bg(cfg.status.accent.into());
    buf.set_stringn(
        inner.x,
        inner.y,
        format!("> {}", query.with_caret(CARET)),
        inner.w as usize,
        Style::default()
            .fg(cfg.status.accent.into())
            .add_modifier(Modifier::BOLD),
    );
    if inner.w == 0 {
        return;
    }
    let hits = App::palette_matches(query.text());
    let sel = sel.min(hits.len().saturating_sub(1));
    for (i, a) in hits.iter().enumerate() {
        let y = inner.y + 2 + i as u16;
        if y >= inner.y + inner.h {
            break;
        }
        let s = if i == sel { sel_style } else { style };
        let line = format!(" {:<width$}", a.to_string(), width = inner.w as usize - 1);
        buf.set_stringn(inner.x, y, line, inner.w as usize, s);
    }
}

fn draw_prompt(buf: &mut Buffer, area: Rect, label: &str, input: &LineEdit, cfg: &Config) {
    let w = area.w.min(60);
    // Five rows: border, the field, a blank, the hint, border.
    let h = 5.min(area.h);
    let rect = Rect::new(
        area.x + (area.w - w) / 2,
        area.y + area.h.saturating_sub(h) / 2,
        w,
        h,
    );
    let inner = modal(buf, rect, label, "Enter to confirm, Esc to cancel", cfg);
    // A 1-row area leaves the border with no interior; set_stringn would then
    // write outside the buffer.
    if inner.h == 0 {
        return;
    }
    buf.set_stringn(
        inner.x,
        inner.y,
        input.with_caret(CARET),
        inner.w as usize,
        Style::default().fg(cfg.status.fg.into()),
    );
}

/// Fold freshly captured images into the standing set for a pane.
///
/// A new image at a cell supersedes the one already there. Without this an
/// animating pane appends one image per pump for ever, and every stale one
/// is re-emitted to the terminal on every frame.
fn merge_images(standing: &mut Vec<Image>, new: Vec<Image>) {
    standing.retain(|i| !new.iter().any(|n| n.row == i.row && n.col == i.col));
    standing.extend(new);
}

// ------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::General;
    use crossterm::event::KeyModifiers;
    use ratatui::backend::TestBackend;

    /// A `Host` with a scripted event queue: an empty queue is a gone source,
    /// which is how these tests end a `main_loop` without a real terminal.
    #[derive(Default)]
    struct FakeHost {
        events: std::collections::VecDeque<Event>,
        out: Vec<u8>,
        polls: usize,
    }

    impl FakeHost {
        fn with(events: Vec<Event>) -> FakeHost {
            FakeHost {
                events: events.into(),
                ..Default::default()
            }
        }
    }

    impl Host for FakeHost {
        fn poll(&mut self, _timeout: Duration) -> Result<Option<Event>> {
            self.polls += 1;
            self.events
                .pop_front()
                .map(Some)
                .ok_or_else(|| anyhow::anyhow!("source gone"))
        }

        fn passthrough(&mut self, bytes: &[u8]) -> Result<()> {
            self.out.extend_from_slice(bytes);
            Ok(())
        }
    }

    fn key(c: char, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), mods))
    }

    /// An app with one tab and one pane. `cat` is a cheap child that stays
    /// alive without writing anything, so nothing races with the assertions.
    fn app() -> App {
        let cfg = Config {
            general: General {
                shell: "/bin/cat".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        App::new(
            cfg,
            PathBuf::from("/dev/null"),
            Rect::new(0, 0, 80, 24),
            String::new(),
        )
        .expect("spawn app")
    }

    fn img(row: u16, col: u16, tag: u8) -> Image {
        Image {
            row,
            col,
            bytes: vec![tag],
        }
    }

    #[test]
    fn a_redrawn_image_replaces_the_one_at_that_cell() {
        let mut standing = vec![];
        for tag in 0..20 {
            merge_images(&mut standing, vec![img(3, 4, tag)]);
        }
        assert_eq!(standing.len(), 1, "one cell accumulated {standing:?}");
        assert_eq!(standing[0].bytes, vec![19], "kept a stale image");

        // A different cell is a different placement, not a replacement.
        merge_images(&mut standing, vec![img(3, 5, 99)]);
        assert_eq!(standing.len(), 2);
    }

    #[test]
    fn a_missing_config_opens_the_welcome_and_answering_it_writes_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ttmux.toml");
        let cfg = Config {
            general: General {
                shell: "/bin/cat".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut a = App::new(cfg, path.clone(), Rect::new(0, 0, 80, 24), String::new()).unwrap();
        assert!(matches!(a.overlay, Overlay::Welcome(_)));

        // Pick the tmux preset, then dismiss the closing screen.
        a.on_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(a.cfg.general.keys_preset, crate::config::KeysPreset::Tmux);
        assert!(path.exists(), "the choice was not written");
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(a.overlay, Overlay::None));

        // The written file is what suppresses the picker next launch.
        let again = App::new(
            Config::load(&path).unwrap(),
            path,
            Rect::new(0, 0, 80, 24),
            String::new(),
        )
        .unwrap();
        assert!(matches!(again.overlay, Overlay::None));
    }

    #[test]
    fn the_rename_prompt_takes_readline_keys() {
        let mut a = app();
        a.tab_mut().name = "old name".into();
        a.dispatch(Action::RenameTab).unwrap();
        // Ctrl-A then Ctrl-K: go to the front and wipe the rest.
        a.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
            .unwrap();
        a.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL))
            .unwrap();
        for c in "new".chars() {
            a.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .unwrap();
        }
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(a.tabs[a.tab].name, "new");
    }

    #[test]
    fn the_rename_pane_prompt_names_the_pane_not_the_tab() {
        let mut a = app();
        a.dispatch(Action::RenamePane).unwrap();
        // The prompt starts on the current title, so clear it first.
        a.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .unwrap();
        for c in "logs".chars() {
            a.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .unwrap();
        }
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        let id = a.focus();
        // An override is what outlasts an escape sequence from the program.
        assert_eq!(a.slots[&id].pane.title_override.as_deref(), Some("logs"));
        assert!(!a.tabs[0].renamed);
    }

    #[test]
    fn a_pane_title_names_the_tab_only_while_the_tab_has_one_pane() {
        let mut a = app();
        let id = a.focus();
        a.slots.get_mut(&id).unwrap().pane.title_override = Some("vim".into());
        assert_eq!(a.tab_label(0, &a.tabs[0]), "1:vim");

        a.dispatch(Action::Split(Dir::Right)).unwrap();
        // The title belongs to a pane now, so the tab keeps its number.
        assert_eq!(a.tab_label(0, &a.tabs[0]), "1");
    }

    #[test]
    fn typing_into_a_pane_asks_for_no_repaint_but_a_command_does() {
        let mut a = app();
        // A wasted frame per keystroke is what this costs when it regresses.
        assert!(
            !a.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
                .unwrap(),
            "a key bound to nothing goes to the pane"
        );
        assert!(
            a.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
                .unwrap(),
            "the prefix shows in the status bar"
        );
    }

    #[test]
    fn frame_and_inner_agree() {
        let mut a = app();
        let id = a.focus();
        let tile = Rect::new(0, 0, 20, 10);

        a.cfg.appearance.gap = 1;
        let (framed, inner) = a.frame(id, tile);
        assert_eq!(framed, Rect::new(1, 1, 18, 8));
        assert_eq!(inner, Rect::new(2, 2, 16, 6));
        assert_eq!(inner, a.inner(id, tile));

        a.cfg.appearance.gap = 0;
        let (framed, inner) = a.frame(id, tile);
        assert_eq!(framed, tile);
        assert_eq!(inner, Rect::new(1, 1, 18, 8));
        assert_eq!(inner, a.inner(id, tile));

        a.cfg.appearance.gap = 1;
        a.cfg.appearance.border_style = BorderStyle::None;
        let (framed, inner) = a.frame(id, tile);
        assert_eq!(framed, Rect::new(1, 1, 18, 8));
        assert_eq!(inner, Rect::new(1, 1, 18, 8));
        assert_eq!(inner, a.inner(id, tile));
    }

    #[test]
    fn a_divider_costs_a_pane_only_the_sides_that_face_another_pane() {
        let mut a = app();
        a.cfg.appearance.border_style = BorderStyle::Divider;
        let left = a.focus();
        a.dispatch(Action::Split(Dir::Right)).unwrap();
        let right = a.focus();

        let rect = |id| a.tabs[a.tab].layout.rect_of(id).unwrap();
        // The left pane touches the edge of the tab on three sides and keeps
        // every cell; the right pane pays for the one line between them.
        assert_eq!(a.inner(left, rect(left)), rect(left));
        let r = rect(right);
        assert_eq!(a.inner(right, r), Rect::new(r.x + 1, r.y, r.w - 1, r.h));
    }

    #[test]
    fn a_config_written_while_it_was_being_read_still_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ttmux.toml");
        std::fs::write(&path, "").unwrap();

        let (cfg, text) = Config::load_stamped(&path);
        // The edit lands after the read, which on a loaded machine can share
        // an mtime with it. The text is what makes it visible.
        std::fs::write(&path, "[general]\nscrollback = 4242\n").unwrap();

        let mut a = App::new(cfg.unwrap(), path, Rect::new(0, 0, 80, 24), text).unwrap();
        a.reload_if_changed();
        assert_eq!(a.cfg.general.scrollback, 4242);
    }

    #[test]
    fn close_pane_finds_the_owning_tab() {
        let mut a = app();
        a.new_tab().unwrap();
        a.new_tab().unwrap();
        let tab0_pane = a.tabs[0].layout.ids()[0];
        let tab1_pane = a.tabs[1].layout.ids()[0];
        a.select_tab(0);

        // Removing a later tab must not shift the selection.
        a.close_tab_at(2);
        assert_eq!(a.tab, 0);
        assert_eq!(a.tabs.len(), 2);

        a.close_pane(tab1_pane);
        assert_eq!(a.tabs.len(), 1);
        assert_eq!(a.tabs[0].layout.ids(), vec![tab0_pane]);
        assert_eq!(a.tabs[0].focus, tab0_pane);
        assert!(a.tab < a.tabs.len());
        for t in &a.tabs {
            for id in t.layout.ids() {
                assert!(a.slots.contains_key(&id), "orphan pane {id} in a layout");
            }
        }
    }

    #[test]
    fn moving_focus_off_a_zoomed_pane_unzooms() {
        let mut a = app();
        let first = a.focus();
        a.split(Dir::Right).unwrap();
        let second = a.focus();
        assert_ne!(first, second);

        a.tab_mut().layout.set_zoom(Some(first));
        a.set_focus(second);
        assert_eq!(a.tabs[0].layout.zoomed, None);
        assert_eq!(a.focus(), second);

        a.tab_mut().layout.set_zoom(Some(first));
        a.set_focus(first);
        assert_eq!(a.tabs[0].layout.zoomed, Some(first));
    }

    #[test]
    fn disabling_agents_clears_their_state() {
        let mut a = app();
        for slot in a.slots.values_mut() {
            slot.state = AgentState::Attention;
        }
        let mut cfg = a.cfg.clone();
        cfg.agents.enabled = false;
        a.apply_config(cfg);
        assert!(a.slots.values().all(|s| s.state == AgentState::Idle));
    }

    #[test]
    fn prompt_survives_a_short_area() {
        let cfg = Config::default();
        for h in [1, 2, 3, 4, 5] {
            let area = Rect::new(0, 0, 20, h);
            let mut buf = Buffer::empty(area.into());
            draw_prompt(&mut buf, area, "rename", &LineEdit::new("x"), &cfg);
        }
    }

    #[test]
    fn each_modal_says_how_to_leave_it() {
        // Every modal is a dead end without its hint: nothing else on screen
        // says which key gets out.
        let cfg = Config::default();
        let area = Rect::new(0, 0, 80, 24);
        let read = |buf: &Buffer| -> String { buf.content().iter().map(|c| c.symbol()).collect() };

        let mut buf = Buffer::empty(area.into());
        draw_help(&mut buf, overlay_rect(area), &cfg, 0);
        assert!(read(&buf).contains("any other key closes"), "help");

        let mut buf = Buffer::empty(area.into());
        draw_palette(&mut buf, overlay_rect(area), &LineEdit::default(), 0, &cfg);
        assert!(read(&buf).contains("Enter to run"), "palette");

        let mut buf = Buffer::empty(area.into());
        draw_prompt(&mut buf, area, "rename tab", &LineEdit::new("x"), &cfg);
        assert!(read(&buf).contains("Esc to cancel"), "prompt");

        let mut buf = Buffer::empty(area.into());
        Settings::new().draw(&mut buf, overlay_rect(area), &cfg);
        let text = read(&buf);
        assert!(text.contains("Esc to close"), "settings");
        assert_eq!(text.matches("Esc to close").count(), 1, "stacked hints");

        let mut buf = Buffer::empty(area.into());
        let w = crate::onboarding::Welcome::new();
        let inner = modal(&mut buf, overlay_rect(area), w.title(), w.hint(), &cfg);
        w.draw(&mut buf, inner, &cfg);
        let text = read(&buf);
        assert!(text.contains("Enter to confirm"), "welcome");
        assert_eq!(text.matches("Enter to confirm").count(), 1, "stacked hints");
    }

    #[test]
    fn palette_survives_a_zero_width_panel() {
        let cfg = Config::default();
        let area = Rect::new(0, 0, 2, 5);
        let mut buf = Buffer::empty(area.into());
        draw_palette(&mut buf, overlay_rect(area), &LineEdit::default(), 0, &cfg);
    }

    #[test]
    fn overlays_hide_the_panes_behind_them() {
        // Overlays are painted over panes already in the buffer, so every cell
        // inside one must be overwritten -- restyling alone leaves the pane's
        // text showing through the overlay's gaps.
        let cfg = Config::default();
        let area = Rect::new(0, 0, 80, 24);
        let rect = overlay_rect(area);
        let filled = || {
            let mut buf = Buffer::empty(area.into());
            for y in area.y..area.bottom() {
                for x in area.x..area.right() {
                    buf.cell_mut((x, y)).unwrap().set_symbol("X");
                }
            }
            buf
        };
        let assert_opaque = |buf: &Buffer, name: &str| {
            for y in rect.y..rect.bottom() {
                for x in rect.x..rect.right() {
                    let sym = buf.cell((x, y)).unwrap().symbol();
                    assert_ne!(sym, "X", "{name} leaked the pane at {x},{y}");
                }
            }
        };

        let mut buf = filled();
        draw_help(&mut buf, rect, &cfg, 0);
        assert_opaque(&buf, "help");

        let mut buf = filled();
        draw_palette(&mut buf, rect, &LineEdit::default(), 0, &cfg);
        assert_opaque(&buf, "palette");

        let mut buf = filled();
        Settings::new().draw(&mut buf, rect, &cfg);
        assert_opaque(&buf, "settings");
    }

    #[test]
    fn overlays_use_a_heavy_border_whatever_the_panes_use() {
        // The doubled stroke is what tells an overlay apart from a pane, so it
        // must not follow the configured pane border style.
        let mut cfg = Config::default();
        cfg.appearance.border_style = crate::config::BorderStyle::Curved;
        let area = Rect::new(0, 0, 80, 24);
        let rect = overlay_rect(area);
        let corner = |buf: &Buffer| buf.cell((rect.x, rect.y)).unwrap().symbol().to_string();

        let mut buf = Buffer::empty(area.into());
        draw_help(&mut buf, rect, &cfg, 0);
        assert_eq!(corner(&buf), "\u{250f}", "help");

        let mut buf = Buffer::empty(area.into());
        Settings::new().draw(&mut buf, rect, &cfg);
        assert_eq!(corner(&buf), "\u{250f}", "settings");
    }

    #[test]
    fn detach_is_an_action_and_exits_without_quitting() {
        assert_eq!("detach".parse(), Ok(Action::Detach));
        assert_eq!(Action::Detach.to_string(), "detach");
        assert!(ALL_ACTIONS.contains(&Action::Detach));

        let mut a = app();
        a.dispatch(Action::Detach).unwrap();
        assert_eq!(a.exit(), Some(Exit::Detached));
        assert!(!a.quit, "detach must not tear the session down");

        let mut a = app();
        a.dispatch(Action::Quit).unwrap();
        assert_eq!(a.exit(), Some(Exit::Quit));
    }

    #[test]
    fn joining_the_last_pane_of_a_window_does_not_land_on_a_gone_tab() {
        // The source window empties and is removed, so every index above it
        // shifts. Three windows, because with two the arithmetic lands on
        // the right answer by accident.
        let mut a = app();
        a.dispatch(Action::NewTab).unwrap();
        a.dispatch(Action::NewTab).unwrap();
        a.select_tab(1);
        assert_eq!(a.tabs.len(), 3);
        let id = a.focus();

        a.move_pane_to_tab(id, 2, false).unwrap();
        assert_eq!(a.tabs.len(), 2, "the emptied window is gone");
        assert_eq!(a.tab, 1, "the view follows the pane, not the hole");
        assert_eq!(a.tabs[1].focus, id);
        assert!(a.tabs[1].layout.ids().contains(&id));
    }

    #[test]
    fn a_pane_joining_a_zoomed_window_is_visible_when_it_lands() {
        let mut a = app();
        a.dispatch(Action::Split(Dir::Right)).unwrap();
        a.dispatch(Action::NewTab).unwrap();
        a.dispatch(Action::Split(Dir::Right)).unwrap();
        a.dispatch(Action::ToggleZoom).unwrap();
        assert!(a.tabs[1].layout.zoomed.is_some());

        a.select_tab(0);
        let id = a.focus();
        a.move_pane_to_tab(id, 1, false).unwrap();
        assert_eq!(a.tabs[1].layout.zoomed, None, "zoom would hide the arrival");
        assert_eq!(a.tabs[1].focus, id);
    }

    #[test]
    fn set_option_writes_a_binding_whose_name_has_a_dot_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ttmux.toml");
        std::fs::write(&path, "[general]\nmouse = true\n").unwrap();
        let mut a = app();
        a.cfg_path = path.clone();
        // No `[keys]` section on disk, and the chord itself holds a dot.
        a.set_option("keys.ctrl+b .", "rename-tab").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.keys.get("ctrl+b ."), Some(&"rename-tab".to_string()));
        assert!(a.set_option("general.nonsense", "1").is_err());
        // Written and read back by the same spelling.
        assert_eq!(
            a.show_options(Some("keys.ctrl+b ."), false).unwrap(),
            "rename-tab"
        );
    }

    #[test]
    fn show_options_prints_json_for_one_key_too() {
        let a = app();
        let out = a.show_options(Some("general.mouse"), true).unwrap();
        assert_eq!(out.trim(), "true");
        let out = a.show_options(Some("general.shell"), true).unwrap();
        assert_eq!(out.trim(), "\"/bin/cat\"", "JSON strings keep their quotes");
        assert_eq!(
            a.show_options(Some("general.shell"), false).unwrap(),
            "/bin/cat"
        );
    }

    #[test]
    fn a_scripted_host_drives_the_loop_with_no_terminal() {
        let mut a = app();
        // The backend is the authority on size, so a resize event only means
        // anything if the terminal behind it really did change.
        let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
        // The default keymap: ctrl+t is the prefix, shift+Q quits.
        let mut host = FakeHost::with(vec![
            Event::Resize(60, 20),
            Event::Paste("hello".into()),
            key('t', KeyModifiers::CONTROL),
            key('Q', KeyModifiers::SHIFT),
            // Never reached: the quit above ends the loop first.
            key('x', KeyModifiers::NONE),
        ]);

        assert_eq!(a.main_loop(&mut term, &mut host).unwrap(), Exit::Quit);
        assert_eq!(a.area, Rect::new(0, 0, 60, 20), "resize reached the app");
        assert_eq!(host.events.len(), 1, "loop stopped at the quit key");
        // Something was actually painted through the backend.
        let painted = term
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|c| c.symbol() != " ");
        assert!(painted);
    }

    #[test]
    fn a_backend_that_shrank_behind_the_app_does_not_draw_past_the_buffer() {
        let mut a = app();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut host = FakeHost::default();
        a.draw(&mut term, &mut host).unwrap();
        // A server's backend follows its narrowest client the moment that
        // client resizes, while the app only learns on the event it has not
        // drained yet. Drawing the old geometry into the new buffer is what
        // put the status row outside it.
        term.backend_mut().resize(40, 8);
        a.draw(&mut term, &mut host).unwrap();
        assert_eq!((a.area.w, a.area.h), (40, 8));
    }

    #[test]
    fn a_gone_host_detaches_rather_than_erroring() {
        let mut a = app();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut host = FakeHost::default();
        assert_eq!(a.main_loop(&mut term, &mut host).unwrap(), Exit::Detached);
    }

    /// An app whose single pane runs `cmd` under `sh`, for tests that need
    /// the guest to set a mode of its own.
    fn app_running(cmd: &str) -> App {
        let cfg = Config {
            general: General {
                shell: "/bin/sh".into(),
                shell_args: vec!["-c".into(), cmd.into()],
                ..Default::default()
            },
            ..Default::default()
        };
        App::new(
            cfg,
            PathBuf::from("/dev/null"),
            Rect::new(0, 0, 80, 24),
            String::new(),
        )
        .expect("spawn app")
    }

    /// Pump the focused pane until `f` holds, or give up after a second.
    fn pump_until(a: &mut App, f: impl Fn(&App) -> bool) -> bool {
        let id = a.focus();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            a.slots.get_mut(&id).unwrap().pane.pump();
            if f(a) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        f(a)
    }

    #[test]
    fn a_paste_is_bracketed_only_when_the_guest_enabled_2004() {
        // The pty echoes what ttmux writes to it, escapes included (as `^[`),
        // so the pane's own screen shows exactly what was sent. The guest
        // only has to stay alive and, in the second case, ask for 2004.
        for (cmd, want) in [
            ("sleep 60", false),
            ("printf '\\033[?2004h'; sleep 60", true),
        ] {
            let mut a = app_running(cmd);
            assert!(
                pump_until(&mut a, |a| a.slots[&a.focus()]
                    .pane
                    .screen()
                    .bracketed_paste()
                    == want),
                "guest never reached bracketed_paste()=={want}"
            );

            let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
            let mut host = FakeHost::with(vec![Event::Paste("hello".into())]);
            assert_eq!(a.main_loop(&mut term, &mut host).unwrap(), Exit::Detached);
            assert!(
                pump_until(&mut a, |a| a.slots[&a.focus()]
                    .pane
                    .screen()
                    .contents()
                    .contains("hello")),
                "the paste never reached the guest"
            );
            let seen = a.slots[&a.focus()].pane.screen().contents();
            for slot in a.slots.values_mut() {
                slot.pane.kill();
            }
            assert_eq!(
                seen.contains("200~"),
                want,
                "guest with 2004={want} saw {seen:?}"
            );
        }
    }

    #[test]
    fn the_frame_is_wrapped_in_synchronized_output() {
        let mut a = app();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut host = FakeHost::default();
        a.draw(&mut term, &mut host).unwrap();
        assert!(
            host.out.starts_with(b"\x1b[?2026h") && host.out.ends_with(b"\x1b[?2026l"),
            "frame not wrapped: {:?}",
            String::from_utf8_lossy(&host.out)
        );
    }

    #[test]
    fn an_image_past_the_pane_bottom_is_clipped_not_dropped() {
        let mut a = app();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut host = FakeHost::default();

        let id = a.focus();
        let inner = a.inner(id, a.tabs[a.tab].layout.rect_of(id).unwrap());
        a.slots.get_mut(&id).unwrap().images.push(Image {
            row: inner.h + 5,
            col: 0,
            bytes: b"PAYLOAD".to_vec(),
        });
        a.draw(&mut term, &mut host).unwrap();

        let out = String::from_utf8_lossy(&host.out).into_owned();
        assert!(out.contains("PAYLOAD"), "overflowing image was dropped");
        let last = format!("\x1b[{};{}H", inner.bottom(), inner.x + 1);
        assert!(out.contains(&last), "not clamped to the last row: {out:?}");
        // The host cursor the frame set has to survive the replay.
        assert!(out.contains('\u{1b}') && out.contains("\x1b7") && out.contains("\x1b8"));
    }

    #[test]
    fn images_and_the_bell_go_to_the_host_not_stdout() {
        let mut a = app();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut host = FakeHost::default();

        let id = a.focus();
        a.slots.get_mut(&id).unwrap().images.push(Image {
            row: 0,
            col: 0,
            bytes: b"\x1b_Gf=100;PAYLOAD\x1b\\".to_vec(),
        });
        a.draw(&mut term, &mut host).unwrap();
        assert!(
            host.out.windows(7).any(|w| w == b"PAYLOAD"),
            "image replay did not reach the host"
        );

        host.out.clear();
        // A pane bell is an attention trigger, and attention rings once.
        a.slots.get_mut(&id).unwrap().pane.bell = true;
        a.update_agents(&mut host).unwrap();
        assert!(host.out.contains(&0x07), "bell did not reach the host");
    }
}
