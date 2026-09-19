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
  `Chord`, `Binding`, `Rgb`, `BorderStyle`, `TitlePosition`, `Bar`, `BarEffect`,
  `KeysPreset`, `preset_keys`. `keys` holds *overrides* on top of the preset;
  `""`/`"none"` unbinds.
- `src/graphics.rs` — lifts kitty (APC), iTerm2 (OSC 1337) and sixel (DCS)
  sequences out of a pane's byte stream before vt100 sees them, so the app can
  replay them to the host terminal.

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
pub enum DragKind {
    Move,
    /// Float edge or corner; each flag is one side that follows the pointer.
    Resize { left: bool, right: bool, top: bool, bottom: bool },
    /// Tiled pane grabbed by the title run of its top border, to snap elsewhere.
    Grab,
    Divider(usize),
    /// Where two perpendicular dividers cross: dragging moves both, which is
    /// what makes a tiled pane resizable by its corner and not only its edges.
    Corner { across: usize, down: usize },
}

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
    /// Mid-drag snap destination for a `Grab`: (target pane, side, the half it
    /// would land in). `None` unless a tiled pane is being dragged over another.
    pub fn snap_target(&self) -> Option<(PaneId, Dir, Rect)>;
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
    /// Graphics sequences captured since the last call, oldest first.
    pub fn take_images(&self) -> Vec<graphics::Image>;
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
/// Encode a mouse event in the guest's declared encoding, dropping the events
/// its declared mode does not report.
pub fn encode_mouse(ev: MouseEvent, col: u16, row: u16, mode: MouseProtocolMode, encoding: MouseProtocolEncoding) -> Option<Vec<u8>>;
```

## src/render.rs
```rust
pub struct Frame { pub tl: char, pub tr: char, pub bl: char, pub br: char, pub h: char, pub v: char }
pub fn frame_chars(style: BorderStyle) -> Frame;
pub fn draw_border(buf: &mut Buffer, rect: Rect, title: &str, focused: bool, alert: bool, zoomed: bool, cfg: &Appearance);
pub fn draw_screen(buf: &mut Buffer, rect: Rect, screen: &vt100::Screen, dim: bool);
pub fn draw_shadow(buf: &mut Buffer, rect: Rect);
/// Tint the half a dragged pane would snap into, keeping the symbols under it.
pub fn draw_snap_preview(buf: &mut Buffer, rect: Rect, accent: Color);
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
/// Draw one row. `cfg` carries the shared colours and effect, `bar` the
/// widget lists for this row; the app calls it once per enabled row.
pub fn draw(buf: &mut Buffer, rect: Rect, cfg: &StatusBar, bar: &Bar, ctx: &Ctx) -> Vec<(usize, std::ops::Range<u16>)>;  // tab hitboxes
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

## src/onboarding.rs
The first-run welcome, shown only when there is no config file. Step one
picks a keymap preset, step two says which key opens help and settings.
Choosing writes the config, which is what stops it appearing twice.

```rust
pub enum Outcome { Continue, Done, Save }
pub struct Welcome { /* step, selection */ }
impl Welcome {
    pub fn new() -> Welcome;
    pub fn on_key(&mut self, ev: KeyEvent, cfg: &mut Config) -> Outcome;
    pub fn draw(&self, buf: &mut Buffer, inner: Rect, cfg: &Config);
    pub fn title(&self) -> &'static str;
}
```

## src/color_picker.rs
Hex colour editing for the settings panel: a swatch grid (the 16 theme
colours, the 6x6x6 cube, the grey ramp), a hex field, per-channel RGB
nudging, and a before/after preview. Every keystroke leaves the new
colour in `value()`, so the running session recolours while you pick.

```rust
pub enum Outcome { Continue, Commit, Cancel }
pub struct Picker { /* focus, grid cursor, hex buffer, original */ }
impl Picker {
    pub fn new(current: &str) -> Picker;   // hex, as the config stores it
    pub fn value(&self) -> String;
    pub fn previous(&self) -> &str;        // what cancel restores
    pub fn on_key(&mut self, ev: KeyEvent) -> Outcome;
    pub fn on_mouse(&mut self, ev: MouseEvent, area: Rect) -> Outcome;
    pub fn draw(&self, buf: &mut Buffer, area: Rect, cfg: &Config);
}
```

## src/server.rs
The session daemon: an `App` with no terminal of its own, forked off the
socket and shared by however many clients attach.

`WireBackend` implements `ratatui::backend::Backend`, so ratatui computes
the cell diff exactly as it does locally and the server only transports it.
It keeps a `Buffer` of what is on screen, which is how a client that
attaches later gets a full repaint without asking the app to redraw.

`Hub` holds the shared state, the event channel and the kill flag. Each
client gets a bounded queue and its own writer thread: the app thread only
ever `try_send`s, and a client that stops reading is dropped rather than
stalling everyone else. The session is as wide and as tall as its
narrowest client, and keeps its geometry when nobody is attached.

Per-client `Detach` drops that one view. `Exit::Detached` out of
`main_loop` is the session's own detach action, so every client leaves and
the panes keep running. Only `Exit::Quit` ends the process.

## src/migrate.rs
`ttmux upgrade`. The client forks a new server. That server gets a
`Snapshot` (tabs, layouts, pane screens) from the old server, and then each
pty master over `SCM_RIGHTS`. It renames its socket over the old one and
says so. Then the old server calls `_exit`, so no destructor kills a shell.
If a step fails before the rename, the old server keeps the session.

## src/mac.rs
A report only. macOS ties the keychain and permission prompts to the audit
session and the responsible process of the server. Both come from the
terminal that started it, and nothing can change them later. `ttmux doctor`
shows them.

## src/client.rs
Connect to the session's socket, or spawn a server and connect to that.
Puts the terminal in raw mode with the alternate screen, mouse and
bracketed paste, sends input on one thread, and applies `Draw` frames
through `CrosstermBackend` so there is no hand-rolled style-to-ANSI code.

## src/app.rs
Owned by the lead. Wires everything: event loop, tabs, panes, dispatch.

The loop is generic over the terminal so the same `App` runs against the
local terminal or against a client on the far end of a socket:

```rust
/// Where a running `App` gets its input and where out-of-band bytes go.
pub trait Host {
    fn poll(&mut self, timeout: Duration) -> anyhow::Result<Option<Event>>;
    /// Bytes for the attached terminal verbatim -- inline-image replays and
    /// the bell. These cannot go through the cell buffer.
    fn passthrough(&mut self, bytes: &[u8]) -> anyhow::Result<()>;
}
/// The local terminal: crossterm's event queue and this process's stdout.
pub struct LocalHost;

/// Quit ends the session and kills every pane; Detached leaves the panes
/// running and only drops this view.
pub enum Exit { Quit, Detached }

impl App {
    pub fn main_loop<B: Backend>(&mut self, term: &mut Terminal<B>, host: &mut dyn Host) -> Result<Exit>;
    pub fn draw<B: Backend>(&mut self, term: &mut Terminal<B>, host: &mut dyn Host) -> Result<()>;
}
pub fn run() -> Result<()>;  // LocalHost + CrosstermBackend, the in-process mode

/// The chrome every modal wears: shadow, raised ground, heavy accent
/// border, title chip, dim hint on the bottom row. Returns the content rect.
pub fn modal(buf: &mut Buffer, rect: Rect, title: &str, hint: &str, cfg: &Config) -> Rect;
/// The content rect inside that chrome, hint row already taken out. Mouse
/// handlers use it too, so hitboxes cannot drift from what was drawn.
pub fn modal_content(rect: Rect) -> Rect;
/// The modal ground, a few steps off `status.bg`. Public because anything
/// that resets a cell inside a modal has to repaint the same ground.
pub fn modal_bg(cfg: &Config) -> Color;
```

Every overlay goes through `modal`, so help, settings, the palette, the
rename prompt, the welcome and the colour picker read as one family and
not as panes. Floating panes keep the plain one-cell shadow: the heavier
one is what marks a modal.

A `poll` that errors is a detach, not a crash: a dead socket means the view
ended.

## src/proto.rs
Length-prefixed JSON over a unix socket, between the client and the server
that owns the ptys.

```rust
pub const PROTOCOL: u32;   // part of the socket path, so a new binary starts
                           // its own server and old clients keep the old one
pub const MAX_FRAME: u32;  // a declared length is never allocated blindly
pub struct WireCell { pub x: u16, pub y: u16, pub symbol: String, pub style: Style }
pub enum ClientMsg { Hello { proto, cols, rows, term }, Input(crossterm::event::Event), Detach, KillServer }
pub enum ServerMsg { Welcome { proto, version }, Draw(Vec<WireCell>), Clear, Cursor(Option<(u16,u16)>), Passthrough(Vec<u8>), Bye(String), Error(String) }

pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()>;
/// `Ok(None)` only at a clean frame boundary, so the caller can tell
/// "peer detached" from "peer died".
pub fn read_msg<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>>;
pub fn socket_dir() -> io::Result<PathBuf>;   // 0700 and owned by you, or it errors
pub fn socket_path(session: &str) -> io::Result<PathBuf>;  // $TTMUX_SOCKET wins
pub fn list_sessions() -> Vec<(String, PathBuf)>;
pub fn is_live(path: &Path) -> bool;
pub fn cleanup_stale();
```

There is no separate resize message: a client's `Event::Resize` is the
resize path, and the server's area is the minimum over attached clients.
