# ttmux architecture

One crate, one module per concern. The public API of each module is listed
below; it is the contract the other modules code against.

`src/app.rs` owns the event loop and wires everything together. Everything it
calls is pure enough to unit test: `layout` is geometry with no I/O, `render`
and `status` only write into a ratatui `Buffer`, `input` is a pure function
from key events to bytes, and `agent` is a state machine over text.

Shared vocabulary:

```rust
pub type PaneId = u32;
pub struct Rect { pub x: u16, pub y: u16, pub w: u16, pub h: u16 }  // in layout.rs
```

`layout::Rect` is ttmux's own type (ratatui's is converted at the render edge
via `From<Rect> for ratatui::layout::Rect`).

Foundations:

- `src/action.rs` — `Action`, `Dir`, `ALL_ACTIONS`. Parse/Display round-trip,
  so config strings and the command palette share one vocabulary.
- `src/config.rs` — `Config` (`general`/`appearance`/`status`/`agents`/`keys`),
  `Chord`, `Binding`, `Rgb`, `BorderStyle`, `StatusPosition`, `TitlePosition`.

## src/layout.rs
Pure geometry. No I/O, no terminal. Fully unit-tested.

```rust
pub type PaneId = u32;
pub struct Rect { pub x: u16, pub y: u16, pub w: u16, pub h: u16 }
impl Rect {
    pub fn new(x: u16, y: u16, w: u16, h: u16) -> Rect;
    pub fn contains(&self, x: u16, y: u16) -> bool;
    pub fn right(&self) -> u16;   // x + w
    pub fn bottom(&self) -> u16;  // y + h
    pub fn shrink(&self, n: u16) -> Rect;  // saturating inset on all sides
    pub fn intersects(&self, other: &Rect) -> bool;
}
impl From<Rect> for ratatui::layout::Rect;
impl From<ratatui::layout::Rect> for Rect;

pub enum Mode { Tiling, Free }
pub enum Preset { EvenHorizontal, EvenVertical, MainVertical, MainHorizontal, Tree }
pub enum DragKind { Move, ResizeEdge(Dir), Divider(usize) }

pub struct Layout { pub mode: Mode, pub preset: Preset, pub zoomed: Option<PaneId>, /* private */ }

impl Layout {
    pub fn new(area: Rect) -> Layout;
    pub fn set_area(&mut self, area: Rect);  // floats keep their fractional geometry
    pub fn area(&self) -> Rect;

    /// False (and no change) when there is no room to split: the caller keeps the pane.
    pub fn insert(&mut self, id: PaneId, near: Option<PaneId>, dir: Option<Dir>) -> bool;
    pub fn remove(&mut self, id: PaneId);
    pub fn ids(&self) -> Vec<PaneId>;            // stable order
    pub fn is_empty(&self) -> bool;

    /// Draw order: tiled panes first, floating panes back-to-front.
    pub fn geometry(&self) -> Vec<(PaneId, Rect)>;
    pub fn rect_of(&self, id: PaneId) -> Option<Rect>;
    pub fn pane_at(&self, x: u16, y: u16) -> Option<PaneId>;  // topmost
    pub fn neighbor(&self, id: PaneId, dir: Dir) -> Option<PaneId>;
    pub fn next(&self, id: PaneId) -> Option<PaneId>;
    pub fn prev(&self, id: PaneId) -> Option<PaneId>;

    pub fn resize(&mut self, id: PaneId, dir: Dir, n: u16);
    pub fn move_pane(&mut self, id: PaneId, dir: Dir, n: u16);  // free mode only
    pub fn swap_next(&mut self, id: PaneId);
    pub fn set_preset(&mut self, p: Preset);
    pub fn set_mode(&mut self, m: Mode);   // Tiling -> Free seeds float rects from tiled ones
    pub fn toggle_float(&mut self, id: PaneId);  // per-pane, valid in either mode
    pub fn is_floating(&self, id: PaneId) -> bool;
    pub fn raise(&mut self, id: PaneId);
    pub fn set_zoom(&mut self, id: Option<PaneId>);

    /// Mouse. `hit_test` says what a press at (x, y) would grab.
    pub fn hit_test(&self, x: u16, y: u16) -> Option<(PaneId, DragKind)>;
    pub fn drag_start(&mut self, x: u16, y: u16) -> bool;
    pub fn drag_to(&mut self, x: u16, y: u16);
    pub fn drag_end(&mut self);
    pub fn dragging(&self) -> bool;
}
```

Rules: nothing ever *creates* a pane smaller than 3x3 (borders included) —
`insert` and docking with `toggle_float` refuse a split that would not leave
both halves at least that big. An `area` too small for the panes it already
holds is arithmetic, not a bug: the tree divides whatever cells exist, so
shrinking the terminal can take tiled panes below 3x3 (`geometry()` still
covers the area exactly). Floats keep at least 3x3 grabbable. Panes never
leave `area`;
`geometry()` covers `area` exactly in tiling mode with no gaps or overlaps
(before `gap` is applied by the renderer); `zoomed` makes `geometry()` return
just that pane filling `area`.

## src/pty.rs
```rust
pub struct Pane {
    pub id: PaneId,
    pub cols: u16, pub rows: u16,
    pub scroll: usize,        // rows scrolled back; 0 = live
    pub bell: bool,           // sticky until cleared by the app
    pub title_override: Option<String>,
}
impl Pane {
    pub fn spawn(id: PaneId, cfg: &Config, cwd: Option<PathBuf>, cols: u16, rows: u16) -> anyhow::Result<Pane>;
    /// Test/headless constructor: runs `cmd` instead of the shell.
    pub fn spawn_cmd(id: PaneId, cmd: CommandBuilder, scrollback: usize, cols: u16, rows: u16) -> anyhow::Result<Pane>;
    pub fn resize(&mut self, cols: u16, rows: u16);
    pub fn send(&mut self, bytes: &[u8]);
    /// Feed pending child output into the emulator. True if anything changed.
    pub fn pump(&mut self) -> bool;
    pub fn screen(&self) -> &vt100::Screen;
    pub fn title(&self) -> String;         // override, else OSC title, else process name
    pub fn cwd(&self) -> Option<PathBuf>;  // for opening new panes in the same dir
    pub fn is_dead(&self) -> bool;
    pub fn exit_status(&self) -> Option<u32>;
    pub fn scroll_by(&mut self, delta: isize);
    pub fn scroll_to_bottom(&mut self);
    /// Recent plain text from the screen, for agent detection.
    pub fn tail(&self, lines: usize) -> String;
    pub fn kill(&mut self);
}
```
Output is read on a background thread into a channel; `pump()` is non-blocking.

## src/agent.rs
```rust
pub enum AgentState { Idle, Busy, Attention, Done }
impl AgentState { pub fn glyph(self) -> &'static str; pub fn is_alert(self) -> bool; }
pub struct Watcher { /* per pane */ }
impl Watcher {
    pub fn new() -> Watcher;
    /// Call on every pump. `bell` is a one-shot OS bell from the pane.
    pub fn update(&mut self, cfg: &config::Agents, title: &str, tail: &str, bell: bool) -> AgentState;
    pub fn state(&self) -> AgentState;
    /// True on the transition into `Attention` (the app rings the bell once).
    pub fn take_alert(&mut self) -> bool;
}
```

## src/input.rs
```rust
pub enum Resolution { Action(Action), Pending, Passthrough }
pub struct Keys { /* prefix state */ }
impl Keys {
    pub fn new(cfg: &Config) -> Keys;
    pub fn reload(&mut self, cfg: &Config);
    pub fn resolve(&mut self, ev: KeyEvent) -> Resolution;
    pub fn pending(&self) -> bool;
}
/// Encode a key event the way a terminal would, for writing to the pty.
pub fn encode_key(ev: KeyEvent, app_cursor_keys: bool) -> Vec<u8>;
/// Encode a mouse event as SGR (1006) for panes that asked for mouse reporting.
pub fn encode_mouse(ev: MouseEvent, col: u16, row: u16) -> Option<Vec<u8>>;
```

## src/render.rs
```rust
pub struct Frame { pub tl: char, pub tr: char, pub bl: char, pub br: char, pub h: char, pub v: char }
pub fn frame_chars(style: BorderStyle) -> Frame;
pub fn draw_border(buf: &mut Buffer, rect: Rect, title: &str, focused: bool, alert: bool, zoomed: bool, cfg: &Appearance);
pub fn draw_screen(buf: &mut Buffer, rect: Rect, screen: &vt100::Screen, dim: bool);
pub fn draw_shadow(buf: &mut Buffer, rect: Rect);
```
`rect` is the *outer* rect including the border; `draw_screen` is given the
inner rect. Both clip to the buffer.

## src/status.rs
```rust
pub struct Ctx<'a> {
    pub session: &'a str, pub mode: &'a str, pub zoomed: bool,
    pub tabs: &'a [(String, bool)],          // (name, active)
    pub panes: &'a [(String, AgentState)],
    pub alerts: usize, pub message: Option<&'a str>, pub pending_prefix: bool,
}
pub const WIDGETS: &[&str];
pub fn draw(buf: &mut Buffer, rect: Rect, cfg: &StatusBar, ctx: &Ctx) -> Vec<(usize, std::ops::Range<u16>)>;  // tab hitboxes
```

## src/settings_ui.rs
```rust
pub enum Outcome { Continue, Close, Apply, Save }
pub struct Settings { /* cursor, section, edit state */ }
impl Settings {
    pub fn new() -> Settings;
    pub fn on_key(&mut self, ev: KeyEvent, cfg: &mut Config) -> Outcome;
    pub fn on_mouse(&mut self, ev: MouseEvent, area: Rect, cfg: &mut Config) -> Outcome;
    pub fn draw(&self, buf: &mut Buffer, area: Rect, cfg: &Config);
}
```

## src/app.rs
Owned by the lead. Wires everything: event loop, tabs, panes, dispatch.
