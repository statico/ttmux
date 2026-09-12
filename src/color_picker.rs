//! The colour picker the settings panel opens for a `Kind::Colour` row.
//!
//! Colours are the one part of the config nobody can hold in their head, so
//! the raw `#rrggbb` entry is replaced by something you can look at. The
//! three ways in are not redundant: the grid is for browsing, the field is
//! for a hex colour copied out of a theme, and the channels are for "close,
//! but greener". Tab moves between them; enter accepts, esc restores.
//!
//! The cube and the grey ramp are emitted as truecolour. A 256-colour
//! terminal quantises them back to the nearest palette entry, so a swatch
//! there lands on a neighbour rather than exactly where it was drawn; the
//! first row stays indexed, which is also why it follows the terminal's own
//! theme instead of a hardcoded ANSI palette.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};

use crate::config::{Config, Rgb};
use crate::layout::Rect;
use crate::settings_ui::put;

/// What the settings panel should do with the picker after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Still open; the (possibly changed) colour is `value()`.
    Continue,
    /// Enter: keep `value()`.
    Commit,
    /// Esc: put `previous()` back.
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Grid,
    Hex,
    Channels,
}

// Row offsets inside the picker's rect. `draw` and `on_mouse` both read them,
// so a click lands on the swatch under the pointer.
const HEX_Y: u16 = 0;
const CHAN_Y: u16 = 1;
const PREVIEW_Y: u16 = 2;
const PREVIEW_H: u16 = 3;
const PREVIEW_W: u16 = 10;
const CAPTION_Y: u16 = PREVIEW_Y + PREVIEW_H;
const GRID_Y: u16 = CAPTION_Y + 1;

/// Rows the picker wants; it clips from the bottom when given fewer.
pub const HEIGHT: u16 = GRID_Y + 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    /// The row's text as it was on open, restored verbatim on cancel: a
    /// named colour or `default` has no hex to round-trip through.
    prev: String,
    cur: Rgb,
    /// The hex field's buffer. Anything `Rgb` parses is allowed, so names
    /// and palette indices stay reachable now that the picker owns the row.
    text: String,
    focus: Focus,
    row: usize,
    col: usize,
    chan: usize,
}

impl Picker {
    pub fn new(current: &str) -> Picker {
        let cur: Rgb = current.parse().unwrap_or_default();
        // Start the cursor on the current colour when it is one of the
        // swatches, so the first arrow press is a step from where you are.
        let (mut row, mut col) = (0, 0);
        for (y, cells) in grid().iter().enumerate() {
            if let Some(x) = cells.iter().position(|c| *c == cur.0) {
                (row, col) = (y, x);
                break;
            }
        }
        Picker {
            prev: current.to_string(),
            cur,
            text: cur.to_string(),
            focus: Focus::Grid,
            row,
            col,
            chan: 0,
        }
    }

    /// The colour as the config would spell it.
    pub fn value(&self) -> String {
        self.cur.to_string()
    }

    pub fn previous(&self) -> &str {
        &self.prev
    }

    pub fn on_key(&mut self, ev: KeyEvent) -> Outcome {
        match ev.code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Enter => return Outcome::Commit,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Grid => Focus::Hex,
                    Focus::Hex => Focus::Channels,
                    Focus::Channels => Focus::Grid,
                }
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Grid => Focus::Channels,
                    Focus::Hex => Focus::Grid,
                    Focus::Channels => Focus::Hex,
                }
            }
            _ => match self.focus {
                Focus::Grid => self.key_grid(ev),
                Focus::Hex => self.key_hex(ev),
                Focus::Channels => self.key_channels(ev),
            },
        }
        Outcome::Continue
    }

    fn key_grid(&mut self, ev: KeyEvent) {
        match ev.code {
            KeyCode::Up => self.step(-1, 0),
            KeyCode::Down => self.step(1, 0),
            KeyCode::Left => self.step(0, -1),
            KeyCode::Right => self.step(0, 1),
            _ => {}
        }
    }

    fn key_hex(&mut self, ev: KeyEvent) {
        match ev.code {
            KeyCode::Backspace => {
                self.text.pop();
            }
            KeyCode::Char(c) => self.text.push(c),
            _ => return,
        }
        // Live: every keystroke that parses moves the preview, and the ones
        // that do not are simply not applied yet.
        if let Ok(c) = self.text.parse::<Rgb>() {
            self.cur = c;
        }
    }

    fn key_channels(&mut self, ev: KeyEvent) {
        let big = ev.modifiers.contains(KeyModifiers::SHIFT);
        match ev.code {
            KeyCode::Char('r') => self.chan = 0,
            KeyCode::Char('g') => self.chan = 1,
            KeyCode::Char('b') => self.chan = 2,
            KeyCode::Up => self.chan = self.chan.saturating_sub(1),
            KeyCode::Down => self.chan = (self.chan + 1).min(2),
            KeyCode::Left => self.nudge(if big { -16 } else { -1 }),
            KeyCode::Right => self.nudge(if big { 16 } else { 1 }),
            _ => {}
        }
    }

    /// Move the grid cursor and take the colour under it.
    fn step(&mut self, dr: isize, dc: isize) {
        let g = grid();
        self.row = (self.row as isize + dr).clamp(0, g.len() as isize - 1) as usize;
        let last = g[self.row].len() as isize - 1;
        self.col = (self.col as isize + dc).clamp(0, last) as usize;
        self.set(g[self.row][self.col]);
    }

    fn nudge(&mut self, delta: i16) {
        let mut c = channels(self.cur.0);
        let v = c[self.chan] as i16 + delta;
        c[self.chan] = v.clamp(0, 255) as u8;
        self.set(Color::Rgb(c[0], c[1], c[2]));
    }

    fn set(&mut self, c: Color) {
        self.cur = Rgb(c);
        self.text = self.cur.to_string();
    }

    pub fn on_mouse(&mut self, ev: MouseEvent, r: Rect) -> Outcome {
        if !matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
            return Outcome::Continue;
        }
        let gy = r.y.saturating_add(GRID_Y);
        if ev.row < gy || ev.column < r.x || !r.contains(ev.column, ev.row) {
            return Outcome::Continue;
        }
        let (row, col) = ((ev.row - gy) as usize, (ev.column - r.x) as usize);
        let g = grid();
        if let Some(c) = g.get(row).and_then(|cells| cells.get(col)) {
            self.focus = Focus::Grid;
            (self.row, self.col) = (row, col);
            self.set(*c);
        }
        Outcome::Continue
    }

    pub fn draw(&self, buf: &mut Buffer, r: Rect, cfg: &Config) {
        let plain = Style::default().fg(cfg.status.fg.into());
        let dim = plain.add_modifier(Modifier::DIM);
        let line = |buf: &mut Buffer, dy: u16, text: &str, style: Style| {
            let y = r.y.saturating_add(dy);
            if dy < r.h {
                put(buf, r.x, y, text, r.w, style);
            }
        };

        let bad = self.text.parse::<Rgb>().is_err();
        let on = |f: Focus| if self.focus == f { "›" } else { " " };
        let cursor = if self.focus == Focus::Hex { "▏" } else { "" };
        line(
            buf,
            HEX_Y,
            &format!("{} hex  {}{cursor}", on(Focus::Hex), self.text),
            if bad { plain.fg(Color::Red) } else { plain },
        );

        let c = channels(self.cur.0);
        let mut chans = format!("{} rgb ", on(Focus::Channels));
        for (i, name) in ["r", "g", "b"].iter().enumerate() {
            let mark = if self.focus == Focus::Channels && i == self.chan {
                '‹'
            } else {
                ' '
            };
            chans.push_str(&format!(" {mark}{name} {:<3}", c[i]));
        }
        line(buf, CHAN_Y, &chans, plain);

        // A block, not a cell: one cell of colour next to text reads as an
        // accident. The old value sits beside it so the change is visible.
        let old: Rgb = self.prev.parse().unwrap_or_default();
        for dy in 0..PREVIEW_H {
            let y = PREVIEW_Y + dy;
            if y >= r.h {
                break;
            }
            put(
                buf,
                r.x,
                r.y + y,
                &" ".repeat(PREVIEW_W as usize),
                PREVIEW_W.min(r.w),
                Style::default().bg(self.cur.0),
            );
            let ox = r.x.saturating_add(PREVIEW_W + 2);
            put(
                buf,
                ox,
                r.y + y,
                &" ".repeat(PREVIEW_W as usize),
                r.right().saturating_sub(ox).min(PREVIEW_W),
                Style::default().bg(old.0),
            );
        }
        line(
            buf,
            CAPTION_Y,
            &format!(
                "{:<w$}was {old}",
                format!("new {}", self.cur),
                w = PREVIEW_W as usize + 2
            ),
            dim,
        );

        for (y, cells) in grid().iter().enumerate() {
            let gy = GRID_Y + y as u16;
            if gy >= r.h {
                break;
            }
            for (x, c) in cells.iter().enumerate() {
                let cx = r.x.saturating_add(x as u16);
                if cx >= r.right() {
                    break;
                }
                let here = self.row == y && self.col == x;
                let (ch, fg) = match (here, self.focus) {
                    (true, Focus::Grid) => ("+", contrast(*c)),
                    (true, _) => ("·", contrast(*c)),
                    _ => (" ", Color::Reset),
                };
                put(buf, cx, r.y + gy, ch, 1, Style::default().bg(*c).fg(fg));
            }
        }
    }
}

/// The swatches, row by row: the 16 theme colours, the 6x6x6 cube one red
/// level per row, then the grey ramp.
fn grid() -> Vec<Vec<Color>> {
    let mut rows = vec![(0..16).map(Color::Indexed).collect::<Vec<_>>()];
    for r in 0..6u8 {
        rows.push((0..36u8).map(|i| xterm(16 + 36 * r + i)).collect());
    }
    rows.push((232..=255u8).map(xterm).collect());
    rows
}

/// A cube or ramp index as truecolour, using xterm's levels.
fn xterm(i: u8) -> Color {
    if i >= 232 {
        let v = 8 + 10 * (i - 232);
        return Color::Rgb(v, v, v);
    }
    const L: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let n = i - 16;
    Color::Rgb(
        L[(n / 36) as usize],
        L[(n / 6 % 6) as usize],
        L[(n % 6) as usize],
    )
}

/// The channels to edit. An indexed or named colour is resolved through the
/// standard palette, and `default` has no colour at all, so it starts black.
fn channels(c: Color) -> [u8; 3] {
    const ANSI: [u32; 16] = [
        0x000000, 0x800000, 0x008000, 0x808000, 0x000080, 0x800080, 0x008080, 0xc0c0c0, 0x808080,
        0xff0000, 0x00ff00, 0xffff00, 0x0000ff, 0xff00ff, 0x00ffff, 0xffffff,
    ];
    let idx = |i: u8| {
        let v = ANSI[i as usize];
        [(v >> 16) as u8, (v >> 8) as u8, v as u8]
    };
    match c {
        Color::Rgb(r, g, b) => [r, g, b],
        Color::Indexed(i) if i < 16 => idx(i),
        Color::Indexed(i) => match xterm(i) {
            Color::Rgb(r, g, b) => [r, g, b],
            _ => [0, 0, 0],
        },
        Color::Black => idx(0),
        Color::Red => idx(1),
        Color::Green => idx(2),
        Color::Yellow => idx(3),
        Color::Blue => idx(4),
        Color::Magenta => idx(5),
        Color::Cyan => idx(6),
        Color::Gray => idx(7),
        Color::DarkGray => idx(8),
        Color::LightRed => idx(9),
        Color::LightGreen => idx(10),
        Color::LightYellow => idx(11),
        Color::LightBlue => idx(12),
        Color::LightMagenta => idx(13),
        Color::LightCyan => idx(14),
        Color::White => idx(15),
        Color::Reset => [0, 0, 0],
    }
}

/// Black or white, whichever stays legible on `c`.
fn contrast(c: Color) -> Color {
    let [r, g, b] = channels(c);
    let lum = 30 * r as u32 + 59 * g as u32 + 11 * b as u32;
    if lum > 12_750 {
        Color::Black
    } else {
        Color::White
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect as TRect;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(p: &mut Picker, s: &str) {
        for ch in s.chars() {
            p.on_key(k(KeyCode::Char(ch)));
        }
    }

    fn tab_to(p: &mut Picker, f: Focus) {
        for _ in 0..3 {
            if p.focus == f {
                return;
            }
            p.on_key(k(KeyCode::Tab));
        }
        panic!("never reached {f:?}");
    }

    #[test]
    fn moving_onto_a_swatch_takes_its_colour() {
        let mut p = Picker::new("#000000");
        p.on_key(k(KeyCode::Down));
        p.on_key(k(KeyCode::Right));
        let Color::Rgb(r, g, b) = grid()[p.row][p.col] else {
            unreachable!()
        };
        assert_eq!(p.value(), format!("#{r:02x}{g:02x}{b:02x}"));
        assert_ne!(p.value(), "#000000");
    }

    #[test]
    fn clicking_a_swatch_picks_it() {
        let mut p = Picker::new("#000000");
        let r = Rect::new(0, 0, 60, 20);
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: GRID_Y + 3,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(p.on_mouse(click, r), Outcome::Continue);
        let Color::Rgb(cr, cg, cb) = grid()[3][5] else {
            unreachable!()
        };
        assert_eq!(p.value(), format!("#{cr:02x}{cg:02x}{cb:02x}"));
    }

    #[test]
    fn a_click_outside_the_grid_changes_nothing() {
        let mut p = Picker::new("#123456");
        let r = Rect::new(0, 0, 60, 20);
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: CHAN_Y,
            modifiers: KeyModifiers::NONE,
        };
        p.on_mouse(click, r);
        assert_eq!(p.value(), "#123456");
    }

    #[test]
    fn typing_hex_sets_the_value_as_it_is_typed() {
        let mut p = Picker::new("#000000");
        tab_to(&mut p, Focus::Hex);
        for _ in 0..7 {
            p.on_key(k(KeyCode::Backspace));
        }
        type_str(&mut p, "#aabbc");
        // Five digits is not a colour yet, so nothing has moved.
        assert_eq!(p.value(), "#000000");
        type_str(&mut p, "c");
        assert_eq!(p.value(), "#aabbcc");
    }

    #[test]
    fn the_field_still_takes_names_and_palette_indices() {
        let mut p = Picker::new("#000000");
        tab_to(&mut p, Focus::Hex);
        for _ in 0..7 {
            p.on_key(k(KeyCode::Backspace));
        }
        type_str(&mut p, "default");
        assert_eq!(p.value(), "default");
    }

    #[test]
    fn a_channel_nudge_moves_that_channel_alone() {
        let mut p = Picker::new("#102030");
        tab_to(&mut p, Focus::Channels);
        p.on_key(k(KeyCode::Char('g')));
        p.on_key(k(KeyCode::Right));
        assert_eq!(p.value(), "#102130");
        p.on_key(k(KeyCode::Left));
        p.on_key(k(KeyCode::Left));
        assert_eq!(p.value(), "#101f30");
        // Shift is the coarse step, and both ends clamp instead of wrapping.
        p.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));
        assert_eq!(p.value(), "#100f30");
        for _ in 0..3 {
            p.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));
        }
        assert_eq!(p.value(), "#100030");
    }

    #[test]
    fn escape_cancels_and_enter_commits() {
        let mut p = Picker::new("#102030");
        p.on_key(k(KeyCode::Down));
        assert_ne!(p.value(), "#102030");
        assert_eq!(p.on_key(k(KeyCode::Esc)), Outcome::Cancel);
        assert_eq!(p.previous(), "#102030");
        assert_eq!(p.on_key(k(KeyCode::Enter)), Outcome::Commit);
    }

    #[test]
    fn an_unparsable_row_opens_on_the_default_colour() {
        let p = Picker::new("nonsense");
        assert_eq!(p.value(), "default");
        assert_eq!(p.previous(), "nonsense");
    }

    #[test]
    fn the_cursor_opens_on_the_colour_already_set() {
        let p = Picker::new("4");
        assert_eq!((p.row, p.col), (0, 4));
    }

    #[test]
    fn a_degenerate_rect_draws_without_panicking() {
        let cfg = Config::default();
        let p = Picker::new("#1e1e2e");
        for (w, h) in [(0, 0), (1, 1), (2, 2), (3, 20), (60, 1), (200, 40)] {
            let mut buf = Buffer::empty(TRect::new(0, 0, w.max(1), h.max(1)));
            p.draw(&mut buf, Rect::new(0, 0, w, h), &cfg);
            // And drawn past the edge of the buffer it owns.
            p.draw(&mut buf, Rect::new(1, 1, w, h), &cfg);
        }
    }
}
