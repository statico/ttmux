//! The application: terminal setup, the event loop, and action dispatch.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::{execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Terminal;

use crate::action::{Action, Dir, ALL_ACTIONS};
use crate::agent::{AgentState, Watcher};
use crate::config::{BorderStyle, Config, StatusPosition};
use crate::input::{encode_key, encode_mouse, Keys, Resolution};
use crate::layout::{Layout, Mode, PaneId, Preset, Rect};
use crate::pty::Pane;
use crate::render;
use crate::settings_ui::{Outcome, Settings};
use crate::status;

/// How long a transient status message stays up.
const MESSAGE_TTL: Duration = Duration::from_secs(4);
/// Event-loop tick. Fast enough to feel live, slow enough to stay idle-cheap.
const TICK: Duration = Duration::from_millis(8);

// ------------------------------------------------------------------ state

struct Slot {
    pane: Pane,
    watcher: Watcher,
    state: AgentState,
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
    Help,
    Settings(Settings),
    Palette { query: String, sel: usize },
    Prompt { label: String, input: String },
}

pub struct App {
    cfg: Config,
    cfg_path: PathBuf,
    keys: Keys,
    tabs: Vec<Tab>,
    tab: usize,
    slots: HashMap<PaneId, Slot>,
    next_id: PaneId,
    overlay: Overlay,
    message: Option<(String, Instant)>,
    /// Hitboxes published by the last status-bar draw, for mouse clicks.
    tab_hits: Vec<(usize, std::ops::Range<u16>)>,
    area: Rect,
    session: String,
    quit: bool,
}

// ------------------------------------------------------------------ entry

pub fn run() -> Result<()> {
    let path = crate::config::config_path();
    let cfg = Config::load(&path).unwrap_or_else(|e| {
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
        let mut app = App::new(cfg, path, Rect::new(0, 0, size.width, size.height))?;
        app.main_loop(&mut term)
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
    pub fn new(cfg: Config, cfg_path: PathBuf, area: Rect) -> Result<App> {
        let keys = Keys::new(&cfg);
        let mut app = App {
            keys,
            cfg,
            cfg_path,
            tabs: vec![],
            tab: 0,
            slots: HashMap::new(),
            next_id: 1,
            overlay: Overlay::None,
            message: None,
            tab_hits: vec![],
            area,
            session: std::env::var("TTMUX_SESSION").unwrap_or_else(|_| "main".into()),
            quit: false,
        };
        app.new_tab()?;
        Ok(app)
    }

    // ---------------------------------------------------------- geometry

    /// The rect panes live in: everything except the status bar.
    fn body(&self) -> Rect {
        let a = self.area;
        match self.cfg.status.position {
            StatusPosition::Hidden => a,
            _ if a.h < 2 => a,
            StatusPosition::Top => Rect::new(a.x, a.y + 1, a.w, a.h - 1),
            StatusPosition::Bottom => Rect::new(a.x, a.y, a.w, a.h - 1),
        }
    }

    fn status_rect(&self) -> Option<Rect> {
        let a = self.area;
        if a.h < 2 {
            return None;
        }
        match self.cfg.status.position {
            StatusPosition::Hidden => None,
            StatusPosition::Top => Some(Rect::new(a.x, a.y, a.w, 1)),
            StatusPosition::Bottom => Some(Rect::new(a.x, a.y + a.h - 1, a.w, 1)),
        }
    }

    /// A pane's tile split into the rect the border is drawn on and the rect
    /// its screen is drawn in. `inner` is also what the pty is sized to, so
    /// these must come from one place or the content is clipped and the mouse
    /// coordinates are off.
    fn frame(&self, outer: Rect) -> (Rect, Rect) {
        (outer.shrink(self.cfg.appearance.gap), self.inner(outer))
    }

    /// The drawable interior of a pane: outer rect minus gap and border.
    fn inner(&self, outer: Rect) -> Rect {
        let outer = outer.shrink(self.cfg.appearance.gap);
        if self.cfg.appearance.border_style == BorderStyle::None {
            outer
        } else {
            outer.shrink(1)
        }
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
            let r = self.inner(outer);
            if let Some(slot) = self.slots.get_mut(&id) {
                let (cols, rows) = (r.w.max(1), r.h.max(1));
                if slot.pane.cols != cols || slot.pane.rows != rows {
                    slot.pane.resize(cols, rows);
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
            },
        );
        Ok(id)
    }

    fn focused_cwd(&self) -> Option<PathBuf> {
        self.slots.get(&self.focus()).and_then(|s| s.pane.cwd())
    }

    fn split(&mut self, dir: Dir) -> Result<()> {
        let cwd = self.focused_cwd();
        let near = self.focus();
        let id = self.spawn_pane(cwd)?;
        let t = self.tab_mut();
        t.layout.set_zoom(None);
        t.layout.insert(id, Some(near), Some(dir));
        t.focus = id;
        self.sync_sizes();
        Ok(())
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
        if i < self.tabs.len() {
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

    fn dispatch(&mut self, action: Action) -> Result<()> {
        use Action::*;
        match action {
            Split(d) => self.split(d)?,
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
            RenameTab => {
                self.overlay = Overlay::Prompt {
                    label: "Rename tab".into(),
                    input: self.tabs[self.tab].name.clone(),
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
                    Overlay::Help => Overlay::None,
                    _ => Overlay::Help,
                }
            }
            CommandPalette => {
                self.overlay = Overlay::Palette {
                    query: String::new(),
                    sel: 0,
                }
            }
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
            Quit => self.quit = true,
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
        self.relayout();
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

    fn on_key(&mut self, ev: KeyEvent) -> Result<()> {
        if ev.kind == KeyEventKind::Release {
            return Ok(());
        }
        // Overlays swallow keys first.
        match &mut self.overlay {
            Overlay::Help => {
                self.overlay = Overlay::None;
                return Ok(());
            }
            Overlay::Settings(s) => {
                let out = s.on_key(ev, &mut self.cfg);
                return self.after_settings(out);
            }
            Overlay::Palette { .. } => return self.palette_key(ev),
            Overlay::Prompt { .. } => return self.prompt_key(ev),
            Overlay::None => {}
        }

        match self.keys.resolve(ev) {
            Resolution::Action(a) => self.dispatch(a),
            Resolution::Pending => Ok(()),
            Resolution::Passthrough => {
                let id = self.focus();
                if let Some(s) = self.slots.get_mut(&id) {
                    // Typing anywhere jumps back to the live view, like a real
                    // terminal.
                    s.pane.scroll_to_bottom();
                    let app_cursor = s.pane.screen().application_cursor();
                    let bytes = encode_key(ev, app_cursor);
                    if !bytes.is_empty() {
                        s.pane.send(&bytes);
                    }
                }
                Ok(())
            }
        }
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
        match ev.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Backspace => {
                query.pop();
                *sel = 0;
            }
            KeyCode::Up => *sel = sel.saturating_sub(1),
            KeyCode::Down => *sel += 1,
            KeyCode::Enter => {
                let hits = Self::palette_matches(query);
                let action = hits.get((*sel).min(hits.len().saturating_sub(1))).copied();
                self.overlay = Overlay::None;
                if let Some(a) = action {
                    self.dispatch(a.clone())?;
                }
            }
            KeyCode::Char(c) => {
                query.push(c);
                *sel = 0;
            }
            _ => {}
        }
        Ok(())
    }

    fn prompt_key(&mut self, ev: KeyEvent) -> Result<()> {
        let Overlay::Prompt { input, .. } = &mut self.overlay else {
            return Ok(());
        };
        match ev.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Enter => {
                let name = input.clone();
                self.overlay = Overlay::None;
                if !name.is_empty() {
                    let t = self.tab_mut();
                    t.name = name;
                    t.renamed = true;
                }
            }
            KeyCode::Char(c) => input.push(c),
            _ => {}
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

        // Status bar: click a tab.
        if let Some(sr) = self.status_rect() {
            if sr.contains(x, y) {
                if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
                    if let Some((i, _)) = self
                        .tab_hits
                        .iter()
                        .find(|(_, r)| r.contains(&x))
                        .map(|(i, r)| (*i, r.clone()))
                    {
                        self.select_tab(i);
                    }
                }
                return Ok(());
            }
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
        let inner = self.inner(outer);
        if !inner.contains(ev.column, ev.row) {
            return;
        }
        if let Some(s) = self.slots.get_mut(&id) {
            if !wants_mouse(s.pane.screen()) {
                return;
            }
            if let Some(bytes) = encode_mouse(ev, ev.column - inner.x, ev.row - inner.y) {
                s.pane.send(&bytes);
            }
        }
    }

    // ------------------------------------------------------------- loop

    fn main_loop(&mut self, term: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        let mut dirty = true;
        let mut last_tick = Instant::now();
        while !self.quit {
            if event::poll(TICK)? {
                match event::read()? {
                    Event::Key(k) => self.on_key(k)?,
                    Event::Mouse(m) => self.on_mouse(m)?,
                    Event::Paste(text) => {
                        let id = self.focus();
                        if let Some(s) = self.slots.get_mut(&id) {
                            // Bracketed paste, so the shell can tell it apart
                            // from typing.
                            s.pane.send(b"\x1b[200~");
                            s.pane.send(text.as_bytes());
                            s.pane.send(b"\x1b[201~");
                        }
                    }
                    Event::Resize(w, h) => {
                        self.area = Rect::new(0, 0, w, h);
                        self.relayout();
                    }
                    _ => {}
                }
                dirty = true;
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
            }
            // The clock widget and the agent busy->idle grace timer both move
            // on their own, so redraw at least once a second regardless of I/O.
            let tick = last_tick.elapsed() >= Duration::from_secs(1);
            if tick {
                last_tick = Instant::now();
                dirty = true;
            }
            if output || tick {
                self.update_agents();
            }
            self.reap();
            if self.quit {
                break;
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
                self.draw(term)?;
                dirty = false;
            }
        }
        Ok(())
    }

    fn update_agents(&mut self) {
        if !self.cfg.agents.enabled {
            return;
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
            let _ = io::stdout().write_all(b"\x07");
            let _ = io::stdout().flush();
        }
    }

    // ------------------------------------------------------------ render

    fn draw(&mut self, term: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        // Snapshot what the closure needs; `draw` borrows `self` mutably.
        let body = self.body();
        if self.tabs.is_empty() {
            return Ok(());
        }
        let mut cursor: Option<(u16, u16)> = None;
        let mut hits = vec![];

        term.draw(|f| {
            let buf = f.buffer_mut();
            buf.set_style(body.into(), Style::default());
            let focus = self.tabs[self.tab].focus;
            let zoomed = self.tabs[self.tab].layout.zoomed.is_some();

            for (id, outer) in self.tabs[self.tab].layout.geometry() {
                let Some(slot) = self.slots.get(&id) else {
                    continue;
                };
                let (framed, inner) = self.frame(outer);
                let floating = self.tabs[self.tab].layout.is_floating(id);
                if floating && self.cfg.appearance.float_shadow {
                    render::draw_shadow(buf, framed);
                }
                let focused = id == focus;
                render::draw_border(
                    buf,
                    framed,
                    &slot.pane.title(),
                    focused,
                    slot.state.is_alert(),
                    zoomed && focused,
                    &self.cfg.appearance,
                );
                render::draw_screen(
                    buf,
                    inner,
                    slot.pane.screen(),
                    self.cfg.appearance.dim_unfocused && !focused,
                );
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

            if let Some(sr) = self.status_rect() {
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
                };
                hits = status::draw(buf, sr, &self.cfg.status, &ctx);
            }

            match &self.overlay {
                Overlay::None => {}
                Overlay::Help => {
                    draw_help(buf, overlay_rect(self.area), &self.cfg);
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
                Overlay::Prompt { label, input } => {
                    draw_prompt(buf, self.area, label, input, &self.cfg);
                    cursor = None;
                }
            }

            if let Some((x, y)) = cursor {
                f.set_cursor_position(Position::new(x, y));
            }
        })?;
        self.tab_hits = hits;
        Ok(())
    }

    fn tab_label(&self, i: usize, t: &Tab) -> String {
        if t.renamed {
            return t.name.clone();
        }
        let title = self
            .slots
            .get(&t.focus)
            .map(|s| s.pane.title())
            .unwrap_or_default();
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

fn panel(buf: &mut Buffer, rect: Rect, title: &str, cfg: &Config) -> Rect {
    let fg: Color = cfg.status.fg.into();
    let bg: Color = cfg.status.bg.into();
    buf.set_style(rect.into(), Style::default().bg(bg).fg(fg));
    render::draw_border(
        buf,
        rect,
        title,
        true,
        false,
        false,
        &crate::config::Appearance {
            border_focused: cfg.status.accent,
            ..cfg.appearance.clone()
        },
    );
    rect.shrink(1)
}

fn draw_help(buf: &mut Buffer, rect: Rect, cfg: &Config) {
    let inner = panel(buf, rect, " help ", cfg);
    let (map, _) = cfg.keymap();
    let style = Style::default().fg(cfg.status.fg.into());
    let accent = Style::default().fg(cfg.status.accent.into());
    for (i, (binding, action)) in map.iter().enumerate() {
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

fn draw_palette(buf: &mut Buffer, rect: Rect, query: &str, sel: usize, cfg: &Config) {
    let inner = panel(buf, rect, " commands ", cfg);
    let style = Style::default().fg(cfg.status.fg.into());
    let sel_style = Style::default()
        .fg(cfg.status.bg.into())
        .bg(cfg.status.accent.into());
    buf.set_stringn(
        inner.x,
        inner.y,
        format!("> {query}"),
        inner.w as usize,
        Style::default()
            .fg(cfg.status.accent.into())
            .add_modifier(Modifier::BOLD),
    );
    if inner.w == 0 {
        return;
    }
    let hits = App::palette_matches(query);
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

fn draw_prompt(buf: &mut Buffer, area: Rect, label: &str, input: &str, cfg: &Config) {
    let w = area.w.min(60);
    let h = 3.min(area.h);
    let rect = Rect::new(
        area.x + (area.w - w) / 2,
        area.y + area.h.saturating_sub(h) / 2,
        w,
        h,
    );
    let inner = panel(buf, rect, label, cfg);
    // A 1-row area leaves the border with no interior; set_stringn would then
    // write outside the buffer.
    if inner.h == 0 {
        return;
    }
    buf.set_stringn(
        inner.x,
        inner.y,
        format!("{input}▏"),
        inner.w as usize,
        Style::default().fg(cfg.status.fg.into()),
    );
}

// ------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::General;

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
        App::new(cfg, PathBuf::from("/dev/null"), Rect::new(0, 0, 80, 24)).expect("spawn app")
    }

    #[test]
    fn frame_and_inner_agree() {
        let mut a = app();
        let tile = Rect::new(0, 0, 20, 10);

        a.cfg.appearance.gap = 1;
        let (framed, inner) = a.frame(tile);
        assert_eq!(framed, Rect::new(1, 1, 18, 8));
        assert_eq!(inner, Rect::new(2, 2, 16, 6));
        assert_eq!(inner, a.inner(tile));

        a.cfg.appearance.gap = 0;
        let (framed, inner) = a.frame(tile);
        assert_eq!(framed, tile);
        assert_eq!(inner, Rect::new(1, 1, 18, 8));
        assert_eq!(inner, a.inner(tile));

        a.cfg.appearance.gap = 1;
        a.cfg.appearance.border_style = BorderStyle::None;
        let (framed, inner) = a.frame(tile);
        assert_eq!(framed, Rect::new(1, 1, 18, 8));
        assert_eq!(inner, Rect::new(1, 1, 18, 8));
        assert_eq!(inner, a.inner(tile));
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
        for h in [1, 2] {
            let area = Rect::new(0, 0, 20, h);
            let mut buf = Buffer::empty(area.into());
            draw_prompt(&mut buf, area, " rename ", "x", &cfg);
        }
    }

    #[test]
    fn palette_survives_a_zero_width_panel() {
        let cfg = Config::default();
        let area = Rect::new(0, 0, 2, 5);
        let mut buf = Buffer::empty(area.into());
        draw_palette(&mut buf, overlay_rect(area), "", 0, &cfg);
    }
}
