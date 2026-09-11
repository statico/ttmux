//! The in-app settings editor: a two-column panel that mutates a live `Config`.
//!
//! The whole point of ttmux is that you never have to read a config-file man
//! page, so every knob in [`Config`] is reachable here. The field table lives
//! in exactly one place — [`fields`] describes a section and [`set`] applies an
//! edit back — so the two can never drift (see `every_field_round_trips`).

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};

use crate::action::{Action, ALL_ACTIONS};
use crate::config::{Binding, BorderStyle, Chord, Config, Rgb, StatusPosition, TitlePosition};
use crate::layout::Rect;

/// What the app should do after handing us an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing happened that the app needs to know about.
    Continue,
    /// Close the overlay.
    Close,
    /// The config changed; re-apply it to the running session.
    Apply,
    /// Write the config to disk.
    Save,
}

pub const SECTIONS: &[&str] = &["General", "Appearance", "Status bar", "Agents", "Keys"];

/// Index of the (dynamic) key-bindings section.
const KEYS: usize = 4;

const BORDER_STYLES: &[&str] = &["curved", "square", "heavy", "double", "dashed", "none"];
const POSITIONS: &[&str] = &["top", "bottom", "hidden"];

/// How a field is edited.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Bool,
    Int { min: i64, max: i64 },
    Text,
    Colour,
    Choice(&'static [&'static str]),
    List,
}

/// One editable row. `label` is empty for key bindings, whose left-hand side
/// is the binding itself and whose value reads `binding → action`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    label: &'static str,
    kind: Kind,
    value: String,
}

// ------------------------------------------------------------- field table

fn bool_f(label: &'static str, v: bool) -> Field {
    Field {
        label,
        kind: Kind::Bool,
        value: v.to_string(),
    }
}
fn int_f(label: &'static str, v: u64, min: i64, max: i64) -> Field {
    Field {
        label,
        kind: Kind::Int { min, max },
        value: v.to_string(),
    }
}
fn text_f(label: &'static str, v: &str) -> Field {
    Field {
        label,
        kind: Kind::Text,
        value: v.to_string(),
    }
}
fn colour_f(label: &'static str, v: Rgb) -> Field {
    Field {
        label,
        kind: Kind::Colour,
        value: v.to_string(),
    }
}
fn list_f(label: &'static str, v: &[String]) -> Field {
    Field {
        label,
        kind: Kind::List,
        value: v.join(", "),
    }
}
fn choice_f(label: &'static str, opts: &'static [&'static str], v: &str) -> Field {
    Field {
        label,
        kind: Kind::Choice(opts),
        value: v.to_string(),
    }
}

fn border_style_name(s: BorderStyle) -> &'static str {
    match s {
        BorderStyle::Curved => "curved",
        BorderStyle::Square => "square",
        BorderStyle::Heavy => "heavy",
        BorderStyle::Double => "double",
        BorderStyle::Dashed => "dashed",
        BorderStyle::None => "none",
    }
}

fn status_position_name(p: StatusPosition) -> &'static str {
    match p {
        StatusPosition::Top => "top",
        StatusPosition::Bottom => "bottom",
        StatusPosition::Hidden => "hidden",
    }
}

fn title_position_name(p: TitlePosition) -> &'static str {
    match p {
        TitlePosition::Top => "top",
        TitlePosition::Bottom => "bottom",
        TitlePosition::Hidden => "hidden",
    }
}

/// Every row of `section`, in display order.
fn fields(cfg: &Config, section: usize) -> Vec<Field> {
    match section {
        0 => {
            let g = &cfg.general;
            vec![
                text_f("shell", &g.shell),
                list_f("shell-args", &g.shell_args),
                bool_f("mouse", g.mouse),
                int_f("scrollback", g.scrollback as u64, 0, 1_000_000),
                bool_f("free-mode", g.free_mode),
                bool_f("focus-follows-mouse", g.focus_follows_mouse),
                int_f("prefix-timeout-ms", g.prefix_timeout_ms, 0, 10_000),
            ]
        }
        1 => {
            let a = &cfg.appearance;
            vec![
                choice_f(
                    "border-style",
                    BORDER_STYLES,
                    border_style_name(a.border_style),
                ),
                colour_f("border", a.border),
                colour_f("border-focused", a.border_focused),
                colour_f("border-alert", a.border_alert),
                choice_f(
                    "title-position",
                    POSITIONS,
                    title_position_name(a.title_position),
                ),
                bool_f("dim-unfocused", a.dim_unfocused),
                int_f("gap", a.gap as u64, 0, 8),
                bool_f("float-shadow", a.float_shadow),
            ]
        }
        2 => {
            let s = &cfg.status;
            vec![
                choice_f("position", POSITIONS, status_position_name(s.position)),
                list_f("left", &s.left),
                list_f("center", &s.center),
                list_f("right", &s.right),
                colour_f("bg", s.bg),
                colour_f("fg", s.fg),
                colour_f("accent", s.accent),
                text_f("separator", &s.separator),
                text_f("time-format", &s.time_format),
            ]
        }
        3 => {
            let a = &cfg.agents;
            vec![
                bool_f("enabled", a.enabled),
                bool_f("bell-on-attention", a.bell_on_attention),
                list_f("attention-patterns", &a.attention_patterns),
                list_f("busy-patterns", &a.busy_patterns),
                list_f("done-patterns", &a.done_patterns),
            ]
        }
        KEYS => cfg
            .keys
            .iter()
            .map(|(k, v)| Field {
                label: "",
                kind: Kind::Text,
                value: format!("{k} → {v}"),
            })
            .collect(),
        _ => vec![],
    }
}

fn parse_bool(v: &str) -> bool {
    matches!(v.trim(), "true" | "yes" | "on" | "1")
}

fn parse_num(v: &str) -> Result<i64, String> {
    v.trim().parse::<i64>().map_err(|e| e.to_string())
}

fn parse_list(v: &str) -> Vec<String> {
    v.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_colour(v: &str) -> Result<Rgb, String> {
    v.parse::<Rgb>()
}

/// Write `value` back into `cfg`. The inverse of [`fields`]; an `Err` is shown
/// to the user inline and nothing is written.
fn set(cfg: &mut Config, section: usize, index: usize, value: &str) -> Result<(), String> {
    match (section, index) {
        (0, 0) => cfg.general.shell = value.to_string(),
        (0, 1) => cfg.general.shell_args = parse_list(value),
        (0, 2) => cfg.general.mouse = parse_bool(value),
        (0, 3) => cfg.general.scrollback = parse_num(value)?.max(0) as usize,
        (0, 4) => cfg.general.free_mode = parse_bool(value),
        (0, 5) => cfg.general.focus_follows_mouse = parse_bool(value),
        (0, 6) => cfg.general.prefix_timeout_ms = parse_num(value)?.max(0) as u64,

        (1, 0) => {
            cfg.appearance.border_style = match value {
                "curved" => BorderStyle::Curved,
                "square" => BorderStyle::Square,
                "heavy" => BorderStyle::Heavy,
                "double" => BorderStyle::Double,
                "dashed" => BorderStyle::Dashed,
                "none" => BorderStyle::None,
                other => return Err(format!("unknown border style: {other}")),
            }
        }
        (1, 1) => cfg.appearance.border = parse_colour(value)?,
        (1, 2) => cfg.appearance.border_focused = parse_colour(value)?,
        (1, 3) => cfg.appearance.border_alert = parse_colour(value)?,
        (1, 4) => {
            cfg.appearance.title_position = match value {
                "top" => TitlePosition::Top,
                "bottom" => TitlePosition::Bottom,
                "hidden" => TitlePosition::Hidden,
                other => return Err(format!("unknown position: {other}")),
            }
        }
        (1, 5) => cfg.appearance.dim_unfocused = parse_bool(value),
        (1, 6) => cfg.appearance.gap = parse_num(value)?.clamp(0, u16::MAX as i64) as u16,
        (1, 7) => cfg.appearance.float_shadow = parse_bool(value),

        (2, 0) => {
            cfg.status.position = match value {
                "top" => StatusPosition::Top,
                "bottom" => StatusPosition::Bottom,
                "hidden" => StatusPosition::Hidden,
                other => return Err(format!("unknown position: {other}")),
            }
        }
        (2, 1) => cfg.status.left = parse_list(value),
        (2, 2) => cfg.status.center = parse_list(value),
        (2, 3) => cfg.status.right = parse_list(value),
        (2, 4) => cfg.status.bg = parse_colour(value)?,
        (2, 5) => cfg.status.fg = parse_colour(value)?,
        (2, 6) => cfg.status.accent = parse_colour(value)?,
        (2, 7) => cfg.status.separator = value.to_string(),
        (2, 8) => cfg.status.time_format = value.to_string(),

        (3, 0) => cfg.agents.enabled = parse_bool(value),
        (3, 1) => cfg.agents.bell_on_attention = parse_bool(value),
        (3, 2) => cfg.agents.attention_patterns = parse_list(value),
        (3, 3) => cfg.agents.busy_patterns = parse_list(value),
        (3, 4) => cfg.agents.done_patterns = parse_list(value),

        (KEYS, i) => {
            let old = cfg
                .keys
                .keys()
                .nth(i)
                .cloned()
                .ok_or_else(|| "no such binding".to_string())?;
            let (b, a) = value
                .split_once('→')
                .ok_or_else(|| "expected `binding → action`".to_string())?;
            let (b, a) = (b.trim(), a.trim());
            b.parse::<Binding>()?;
            a.parse::<Action>()?;
            if b != old && cfg.keys.contains_key(b) {
                return Err(format!("{b} is already bound"));
            }
            cfg.keys.remove(&old);
            cfg.keys.insert(b.to_string(), a.to_string());
        }
        _ => return Err("no such field".into()),
    }
    Ok(())
}

// ----------------------------------------------------------------- geometry

/// Where the pieces of the panel land. Shared by `draw` and `on_mouse` so a
/// click always hits the row the user is looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Geom {
    sections: Rect,
    /// Rows available for fields (footer and error line excluded).
    fields: Rect,
    error_y: u16,
    footer_y: u16,
}

fn geometry(area: Rect) -> Geom {
    let inner = area.shrink(1);
    let sec_w = inner.w.min(16);
    let fx = inner.x.saturating_add(sec_w).saturating_add(1);
    let fw = inner.right().saturating_sub(fx);
    // Bottom two rows are the error line and the hint footer.
    let rows = inner.h.saturating_sub(2);
    Geom {
        sections: Rect::new(inner.x, inner.y, sec_w, rows),
        fields: Rect::new(fx, inner.y, fw, rows),
        error_y: inner.y.saturating_add(rows),
        footer_y: inner.bottom().saturating_sub(1),
    }
}

// ------------------------------------------------------------------- state

#[derive(Debug, Clone, PartialEq, Eq)]
enum Edit {
    None,
    /// Text/colour/number/list buffer for the selected row.
    Buffer(String),
    /// Waiting for a key to rebind the selected row.
    Capture,
    /// Waiting for a key for a brand new binding.
    CaptureNew,
    /// Got the key, now choosing the action.
    PickAction {
        chord: String,
        filter: String,
        sel: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sections,
    Fields,
}

/// The settings overlay.
#[derive(Debug, Clone)]
pub struct Settings {
    section: usize,
    row: usize,
    focus: Focus,
    /// Wheel scroll offset; clamped to keep the selected row visible.
    scroll: usize,
    edit: Edit,
    error: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self::new()
    }
}

impl Settings {
    pub fn new() -> Settings {
        Settings {
            section: 0,
            row: 0,
            focus: Focus::Fields,
            scroll: 0,
            edit: Edit::None,
            error: None,
        }
    }

    fn editing(&self) -> bool {
        self.edit != Edit::None
    }

    /// Scroll offset actually used, given a viewport of `rows` for `total` rows.
    fn eff_scroll(&self, rows: usize, total: usize) -> usize {
        if rows == 0 {
            return 0;
        }
        let max = total.saturating_sub(rows);
        let mut s = self.scroll.min(max);
        if self.row < s {
            s = self.row;
        } else if self.row >= s + rows {
            s = self.row + 1 - rows;
        }
        s
    }

    fn clamp_row(&mut self, cfg: &Config) {
        let n = fields(cfg, self.section).len();
        self.row = self.row.min(n.saturating_sub(1));
    }

    // ------------------------------------------------------------- keyboard

    pub fn on_key(&mut self, ev: KeyEvent, cfg: &mut Config) -> Outcome {
        match self.edit.clone() {
            Edit::Buffer(buf) => self.key_buffer(ev, cfg, buf),
            Edit::Capture => self.key_capture(ev, cfg),
            Edit::CaptureNew => self.key_capture_new(ev, cfg),
            Edit::PickAction { chord, filter, sel } => self.key_pick(ev, cfg, chord, filter, sel),
            Edit::None => self.key_normal(ev, cfg),
        }
    }

    fn key_normal(&mut self, ev: KeyEvent, cfg: &mut Config) -> Outcome {
        let fs = fields(cfg, self.section);
        self.row = self.row.min(fs.len().saturating_sub(1));
        let kind = fs.get(self.row).map(|f| f.kind.clone());

        match ev.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Char('s') => return Outcome::Save,
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Sections => Focus::Fields,
                    Focus::Fields => Focus::Sections,
                };
            }
            KeyCode::Up | KeyCode::Char('k') => match self.focus {
                Focus::Sections => {
                    self.section = self.section.saturating_sub(1);
                    self.row = 0;
                    self.scroll = 0;
                }
                Focus::Fields => self.row = self.row.saturating_sub(1),
            },
            KeyCode::Down | KeyCode::Char('j') => match self.focus {
                Focus::Sections => {
                    self.section = (self.section + 1).min(SECTIONS.len() - 1);
                    self.row = 0;
                    self.scroll = 0;
                }
                Focus::Fields => {
                    if self.row + 1 < fs.len() {
                        self.row += 1;
                    }
                }
            },
            KeyCode::Left => match (self.focus, &kind) {
                (Focus::Fields, Some(Kind::Choice(opts))) => return self.cycle(cfg, opts, -1),
                (Focus::Fields, Some(Kind::Int { min, max })) => {
                    return self.bump(cfg, *min, *max, -1)
                }
                _ => self.focus = Focus::Sections,
            },
            KeyCode::Right => match (self.focus, &kind) {
                (Focus::Sections, _) => self.focus = Focus::Fields,
                (Focus::Fields, Some(Kind::Choice(opts))) => return self.cycle(cfg, opts, 1),
                (Focus::Fields, Some(Kind::Int { min, max })) => {
                    return self.bump(cfg, *min, *max, 1)
                }
                _ => {}
            },
            KeyCode::Char(' ') | KeyCode::Enter => {
                if self.focus == Focus::Sections {
                    self.focus = Focus::Fields;
                    return Outcome::Continue;
                }
                self.error = None;
                match kind {
                    Some(Kind::Bool) => {
                        let now = fs[self.row].value == "true";
                        let v = (!now).to_string();
                        return self.apply(cfg, &v);
                    }
                    Some(Kind::Choice(opts)) => return self.cycle(cfg, opts, 1),
                    Some(_) if self.section == KEYS => self.edit = Edit::Capture,
                    Some(_) => self.edit = Edit::Buffer(fs[self.row].value.clone()),
                    None => {}
                }
            }
            KeyCode::Char('d') if self.section == KEYS => {
                if let Some(k) = cfg.keys.keys().nth(self.row).cloned() {
                    cfg.keys.remove(&k);
                    self.clamp_row(cfg);
                    return Outcome::Apply;
                }
            }
            KeyCode::Char('a') if self.section == KEYS => {
                self.error = None;
                self.edit = Edit::CaptureNew;
            }
            _ => {}
        }
        Outcome::Continue
    }

    /// Step a `Choice` field by `delta`, wrapping.
    fn cycle(&mut self, cfg: &mut Config, opts: &[&str], delta: isize) -> Outcome {
        let cur = fields(cfg, self.section)[self.row].value.clone();
        let at = opts.iter().position(|o| *o == cur).unwrap_or(0) as isize;
        let n = opts.len() as isize;
        let next = opts[(at + delta).rem_euclid(n) as usize].to_string();
        self.apply(cfg, &next)
    }

    /// Step an `Int` field, clamped to its range.
    fn bump(&mut self, cfg: &mut Config, min: i64, max: i64, delta: i64) -> Outcome {
        let cur: i64 = fields(cfg, self.section)[self.row]
            .value
            .parse()
            .unwrap_or(min);
        let step = ((max - min) / 100).max(1);
        let next = (cur + delta * step).clamp(min, max);
        self.apply(cfg, &next.to_string())
    }

    /// Apply a value to the selected row, recording any error inline.
    fn apply(&mut self, cfg: &mut Config, value: &str) -> Outcome {
        match set(cfg, self.section, self.row, value) {
            Ok(()) => {
                self.error = None;
                Outcome::Apply
            }
            Err(e) => {
                self.error = Some(e);
                Outcome::Continue
            }
        }
    }

    fn key_buffer(&mut self, ev: KeyEvent, cfg: &mut Config, mut buf: String) -> Outcome {
        match ev.code {
            KeyCode::Esc => {
                self.edit = Edit::None;
                self.error = None;
            }
            KeyCode::Enter => match set(cfg, self.section, self.row, &buf) {
                Ok(()) => {
                    self.edit = Edit::None;
                    self.error = None;
                    return Outcome::Apply;
                }
                Err(e) => self.error = Some(e),
            },
            KeyCode::Backspace => {
                buf.pop();
                self.edit = Edit::Buffer(buf);
            }
            KeyCode::Char(c) => {
                buf.push(c);
                self.edit = Edit::Buffer(buf);
            }
            _ => {}
        }
        Outcome::Continue
    }

    fn key_capture(&mut self, ev: KeyEvent, cfg: &mut Config) -> Outcome {
        self.edit = Edit::None;
        if ev.code == KeyCode::Esc {
            return Outcome::Continue;
        }
        let chord = Chord::from_event(ev).to_string();
        let fs = fields(cfg, KEYS);
        let Some(f) = fs.get(self.row) else {
            return Outcome::Continue;
        };
        let action = f
            .value
            .split_once('→')
            .map(|(_, a)| a.trim().to_string())
            .unwrap_or_default();
        let out = self.apply(cfg, &format!("{chord} → {action}"));
        if out == Outcome::Apply {
            // The map is sorted by binding, so the row may have moved.
            if let Some(i) = cfg.keys.keys().position(|k| *k == chord) {
                self.row = i;
            }
        }
        out
    }

    fn key_capture_new(&mut self, ev: KeyEvent, cfg: &mut Config) -> Outcome {
        if ev.code == KeyCode::Esc {
            self.edit = Edit::None;
            return Outcome::Continue;
        }
        let chord = Chord::from_event(ev).to_string();
        if cfg.keys.contains_key(&chord) {
            self.edit = Edit::None;
            self.error = Some(format!("{chord} is already bound"));
            return Outcome::Continue;
        }
        self.edit = Edit::PickAction {
            chord,
            filter: String::new(),
            sel: 0,
        };
        Outcome::Continue
    }

    fn key_pick(
        &mut self,
        ev: KeyEvent,
        cfg: &mut Config,
        chord: String,
        mut filter: String,
        mut sel: usize,
    ) -> Outcome {
        let matches = filtered_actions(&filter);
        match ev.code {
            KeyCode::Esc => self.edit = Edit::None,
            KeyCode::Up => sel = sel.saturating_sub(1),
            KeyCode::Down => sel = (sel + 1).min(matches.len().saturating_sub(1)),
            KeyCode::Backspace => {
                filter.pop();
                sel = 0;
            }
            KeyCode::Enter => {
                self.edit = Edit::None;
                if let Some(a) = matches.get(sel) {
                    cfg.keys.insert(chord.clone(), a.to_string());
                    if let Some(i) = cfg.keys.keys().position(|k| *k == chord) {
                        self.section = KEYS;
                        self.row = i;
                    }
                    self.error = None;
                    return Outcome::Apply;
                }
                return Outcome::Continue;
            }
            KeyCode::Char(c) => {
                filter.push(c);
                sel = 0;
            }
            _ => {}
        }
        if self.edit != Edit::None {
            self.edit = Edit::PickAction { chord, filter, sel };
        }
        Outcome::Continue
    }

    // ---------------------------------------------------------------- mouse

    pub fn on_mouse(&mut self, ev: MouseEvent, area: Rect, cfg: &mut Config) -> Outcome {
        let g = geometry(area);
        let rows = g.fields.h as usize;
        let total = fields(cfg, self.section).len();
        match ev.kind {
            MouseEventKind::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            MouseEventKind::ScrollDown => {
                self.scroll = (self.scroll + 1).min(total.saturating_sub(rows));
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.editing() {
                    return Outcome::Continue;
                }
                if g.sections.contains(ev.column, ev.row) {
                    let i = (ev.row - g.sections.y) as usize;
                    if i < SECTIONS.len() {
                        self.focus = Focus::Sections;
                        self.section = i;
                        self.row = 0;
                        self.scroll = 0;
                    }
                } else if g.fields.contains(ev.column, ev.row) {
                    let i = self.eff_scroll(rows, total) + (ev.row - g.fields.y) as usize;
                    if i < total {
                        self.focus = Focus::Fields;
                        self.row = i;
                        self.error = None;
                        if fields(cfg, self.section)[i].kind == Kind::Bool {
                            let now = fields(cfg, self.section)[i].value == "true";
                            let v = (!now).to_string();
                            return self.apply(cfg, &v);
                        }
                    }
                }
            }
            _ => {}
        }
        Outcome::Continue
    }

    // ----------------------------------------------------------------- draw

    pub fn draw(&self, buf: &mut Buffer, area: Rect, cfg: &Config) {
        if area.w < 4 || area.h < 3 {
            return;
        }
        let base = Style::default();
        border(buf, area, base);
        let g = geometry(area);

        // Section list.
        for (i, name) in SECTIONS.iter().enumerate() {
            let y = g.sections.y + i as u16;
            if y >= g.sections.bottom() {
                break;
            }
            let style = if i == self.section {
                if self.focus == Focus::Sections {
                    base.add_modifier(Modifier::REVERSED)
                } else {
                    base.add_modifier(Modifier::BOLD)
                }
            } else {
                base
            };
            let text = format!("{:<width$}", name, width = g.sections.w as usize);
            put(buf, g.sections.x, y, &text, g.sections.w, style);
        }
        // Divider between the columns.
        for y in g.sections.y..g.sections.bottom() {
            put(buf, g.sections.right(), y, "│", 1, base);
        }

        if let Edit::PickAction { filter, sel, .. } = &self.edit {
            self.draw_picker(buf, g.fields, filter, *sel, base);
        } else {
            self.draw_fields(buf, g.fields, cfg, base);
        }

        if let Some(e) = &self.error {
            put(
                buf,
                g.fields.x,
                g.error_y,
                e,
                g.fields.w,
                base.fg(Color::Red),
            );
        }
        let hint = match self.edit {
            Edit::Capture | Edit::CaptureNew => "press a key…  esc cancel",
            Edit::Buffer(_) => "type to edit  enter commit  esc cancel",
            _ => "↑↓ move  ←→ change  enter edit  s save  esc close",
        };
        let inner = area.shrink(1);
        put(
            buf,
            inner.x,
            g.footer_y,
            hint,
            inner.w,
            base.fg(Color::DarkGray),
        );
    }

    fn draw_fields(&self, buf: &mut Buffer, r: Rect, cfg: &Config, base: Style) {
        let fs = fields(cfg, self.section);
        let rows = r.h as usize;
        let scroll = self.eff_scroll(rows, fs.len());
        for (i, f) in fs.iter().enumerate().skip(scroll).take(rows) {
            let y = r.y + (i - scroll) as u16;
            let selected = i == self.row && self.focus == Focus::Fields;
            let (label, mut value) = match f.label {
                "" => match f.value.split_once('→') {
                    Some((b, a)) => (b.trim().to_string(), a.trim().to_string()),
                    None => (f.value.clone(), String::new()),
                },
                l => (l.to_string(), f.value.clone()),
            };
            if selected {
                match &self.edit {
                    Edit::Buffer(b) => value = format!("{b}▏"),
                    Edit::Capture => value = "press a key…".into(),
                    _ => {}
                }
            }
            let style = if selected {
                base.add_modifier(Modifier::REVERSED)
            } else {
                base
            };
            put(buf, r.x, y, &row_text(&label, &value, r.w), r.w, style);
        }
    }

    fn draw_picker(&self, buf: &mut Buffer, r: Rect, filter: &str, sel: usize, base: Style) {
        put(buf, r.x, r.y, &format!("action: {filter}▏"), r.w, base);
        for (i, a) in filtered_actions(filter)
            .iter()
            .enumerate()
            .take(r.h.saturating_sub(1) as usize)
        {
            let style = if i == sel {
                base.add_modifier(Modifier::REVERSED)
            } else {
                base
            };
            put(buf, r.x, r.y + 1 + i as u16, a, r.w, style);
        }
    }
}

/// `label ..... value`, padded to `w`.
fn row_text(label: &str, value: &str, w: u16) -> String {
    let w = w as usize;
    let (l, v) = (label.chars().count(), value.chars().count());
    if l + v + 2 >= w {
        let mut s = format!("{label} {value}");
        s.truncate_chars(w);
        return s;
    }
    format!("{label} {} {value}", ".".repeat(w - l - v - 2))
}

trait TruncateChars {
    fn truncate_chars(&mut self, n: usize);
}
impl TruncateChars for String {
    fn truncate_chars(&mut self, n: usize) {
        if let Some((i, _)) = self.char_indices().nth(n) {
            self.truncate(i);
        }
    }
}

fn filtered_actions(filter: &str) -> Vec<String> {
    let f = filter.trim().to_ascii_lowercase();
    ALL_ACTIONS
        .iter()
        .map(|a| a.to_string())
        .filter(|s| f.is_empty() || s.to_ascii_lowercase().contains(&f))
        .collect()
}

/// Plain box-drawing border (the renderer's fancier one may not exist yet).
fn border(buf: &mut Buffer, r: Rect, style: Style) {
    let (x1, y1) = (r.right().saturating_sub(1), r.bottom().saturating_sub(1));
    for x in r.x..r.right() {
        put(buf, x, r.y, "─", 1, style);
        put(buf, x, y1, "─", 1, style);
    }
    for y in r.y..r.bottom() {
        put(buf, r.x, y, "│", 1, style);
        put(buf, x1, y, "│", 1, style);
    }
    put(buf, r.x, r.y, "┌", 1, style);
    put(buf, x1, r.y, "┐", 1, style);
    put(buf, r.x, y1, "└", 1, style);
    put(buf, x1, y1, "┘", 1, style);
    put(
        buf,
        r.x + 1,
        r.y,
        " settings ",
        r.w.saturating_sub(2),
        style,
    );
}

/// Clipped single-line write. Never panics on a small buffer.
///
/// ponytail: one column per char; wide (CJK) glyphs would need unicode-width.
fn put(buf: &mut Buffer, x: u16, y: u16, s: &str, max: u16, style: Style) {
    let a = buf.area;
    if y < a.y || y >= a.bottom() || max == 0 {
        return;
    }
    let limit = x.saturating_add(max).min(a.right());
    let mut cx = x;
    for ch in s.chars() {
        if cx >= limit {
            break;
        }
        if cx >= a.x {
            if let Some(cell) = buf.cell_mut((cx, y)) {
                cell.set_char(ch);
                cell.set_style(style);
            }
        }
        cx = cx.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::layout::Rect as TRect;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn c(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
    }

    /// Jump straight at a field by label, the way the user would arrow to it.
    fn goto(s: &mut Settings, cfg: &Config, section: usize, label: &str) {
        s.section = section;
        s.focus = Focus::Fields;
        s.row = fields(cfg, section)
            .iter()
            .position(|f| f.label == label)
            .unwrap_or_else(|| panic!("no field {label}"));
    }

    fn type_str(s: &mut Settings, cfg: &mut Config, text: &str) {
        for ch in text.chars() {
            s.on_key(c(ch), cfg);
        }
    }

    fn click(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn space_toggles_a_bool_and_applies() {
        let (mut s, mut cfg) = (Settings::new(), Config::default());
        goto(&mut s, &cfg, 0, "mouse");
        assert!(cfg.general.mouse);
        assert_eq!(s.on_key(k(KeyCode::Char(' ')), &mut cfg), Outcome::Apply);
        assert!(!cfg.general.mouse);
        assert_eq!(s.on_key(k(KeyCode::Enter), &mut cfg), Outcome::Apply);
        assert!(cfg.general.mouse);
    }

    #[test]
    fn int_field_steps_and_clamps() {
        let (mut s, mut cfg) = (Settings::new(), Config::default());
        goto(&mut s, &cfg, 0, "scrollback");
        s.on_key(k(KeyCode::Right), &mut cfg);
        assert!(cfg.general.scrollback > 10_000);

        cfg.general.scrollback = 1_000_000;
        s.on_key(k(KeyCode::Right), &mut cfg);
        assert_eq!(cfg.general.scrollback, 1_000_000);

        cfg.general.scrollback = 0;
        s.on_key(k(KeyCode::Left), &mut cfg);
        assert_eq!(cfg.general.scrollback, 0);
    }

    #[test]
    fn choice_cycles_and_wraps() {
        let (mut s, mut cfg) = (Settings::new(), Config::default());
        goto(&mut s, &cfg, 1, "border-style");
        let seen: Vec<BorderStyle> = (0..5)
            .map(|_| {
                s.on_key(k(KeyCode::Right), &mut cfg);
                cfg.appearance.border_style
            })
            .collect();
        assert_eq!(
            seen,
            vec![
                BorderStyle::Square,
                BorderStyle::Heavy,
                BorderStyle::Double,
                BorderStyle::Dashed,
                BorderStyle::None,
            ]
        );
        s.on_key(k(KeyCode::Right), &mut cfg);
        assert_eq!(cfg.appearance.border_style, BorderStyle::Curved);
        s.on_key(k(KeyCode::Left), &mut cfg);
        assert_eq!(cfg.appearance.border_style, BorderStyle::None);
    }

    #[test]
    fn colour_edit_commits_valid_and_rejects_junk() {
        let (mut s, mut cfg) = (Settings::new(), Config::default());
        goto(&mut s, &cfg, 1, "border");
        s.on_key(k(KeyCode::Enter), &mut cfg);
        for _ in 0..8 {
            s.on_key(k(KeyCode::Backspace), &mut cfg);
        }
        type_str(&mut s, &mut cfg, "#ff0000");
        assert_eq!(s.on_key(k(KeyCode::Enter), &mut cfg), Outcome::Apply);
        assert_eq!(cfg.appearance.border.to_string(), "#ff0000");

        s.on_key(k(KeyCode::Enter), &mut cfg);
        for _ in 0..8 {
            s.on_key(k(KeyCode::Backspace), &mut cfg);
        }
        type_str(&mut s, &mut cfg, "nonsense");
        assert_eq!(s.on_key(k(KeyCode::Enter), &mut cfg), Outcome::Continue);
        assert_eq!(cfg.appearance.border.to_string(), "#ff0000");
        assert!(s.error.is_some());
    }

    #[test]
    fn esc_cancels_a_text_edit() {
        let (mut s, mut cfg) = (Settings::new(), Config::default());
        goto(&mut s, &cfg, 2, "time-format");
        s.on_key(k(KeyCode::Enter), &mut cfg);
        type_str(&mut s, &mut cfg, "xyz");
        assert_eq!(s.on_key(k(KeyCode::Esc), &mut cfg), Outcome::Continue);
        assert_eq!(cfg.status.time_format, "%H:%M");
        assert!(!s.editing());
    }

    #[test]
    fn list_field_splits_on_commas() {
        let (mut s, mut cfg) = (Settings::new(), Config::default());
        goto(&mut s, &cfg, 2, "left");
        s.on_key(k(KeyCode::Enter), &mut cfg);
        for _ in 0..40 {
            s.on_key(k(KeyCode::Backspace), &mut cfg);
        }
        type_str(&mut s, &mut cfg, "session, time,");
        assert_eq!(s.on_key(k(KeyCode::Enter), &mut cfg), Outcome::Apply);
        assert_eq!(cfg.status.left, vec!["session".to_string(), "time".into()]);
    }

    #[test]
    fn key_capture_rebinds_and_drops_the_old_key() {
        let (mut s, mut cfg) = (Settings::new(), Config::default());
        s.section = KEYS;
        s.focus = Focus::Fields;
        s.row = cfg.keys.keys().position(|k| k == "alt+left").unwrap();
        let action = cfg.keys["alt+left"].clone();

        s.on_key(k(KeyCode::Enter), &mut cfg);
        let out = s.on_key(KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE), &mut cfg);
        assert_eq!(out, Outcome::Apply);
        assert!(!cfg.keys.contains_key("alt+left"));
        assert_eq!(cfg.keys["f9"], action);
    }

    #[test]
    fn key_capture_refuses_to_shadow() {
        let (mut s, mut cfg) = (Settings::new(), Config::default());
        let before = cfg.keys.clone();
        s.section = KEYS;
        s.row = cfg.keys.keys().position(|k| k == "alt+left").unwrap();
        s.on_key(k(KeyCode::Enter), &mut cfg);
        let out = s.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT), &mut cfg);
        assert_eq!(out, Outcome::Continue);
        assert!(s.error.is_some());
        assert_eq!(cfg.keys, before);
    }

    #[test]
    fn d_deletes_a_binding() {
        let (mut s, mut cfg) = (Settings::new(), Config::default());
        s.section = KEYS;
        s.row = cfg.keys.keys().position(|k| k == "alt+left").unwrap();
        let n = cfg.keys.len();
        assert_eq!(s.on_key(c('d'), &mut cfg), Outcome::Apply);
        assert!(!cfg.keys.contains_key("alt+left"));
        assert_eq!(cfg.keys.len(), n - 1);
    }

    #[test]
    fn a_adds_a_binding_through_the_action_picker() {
        let (mut s, mut cfg) = (Settings::new(), Config::default());
        s.section = KEYS;
        s.on_key(c('a'), &mut cfg);
        s.on_key(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE), &mut cfg);
        type_str(&mut s, &mut cfg, "quit");
        assert_eq!(s.on_key(k(KeyCode::Enter), &mut cfg), Outcome::Apply);
        assert_eq!(cfg.keys["f12"], "quit");
        assert!(!s.editing());
    }

    #[test]
    fn save_and_close() {
        let (mut s, mut cfg) = (Settings::new(), Config::default());
        assert_eq!(s.on_key(c('s'), &mut cfg), Outcome::Save);
        assert_eq!(s.on_key(k(KeyCode::Esc), &mut cfg), Outcome::Close);
    }

    #[test]
    fn every_field_round_trips_through_the_setter() {
        for section in 0..SECTIONS.len() {
            let mut cfg = Config::default();
            let before = cfg.clone();
            for (i, f) in fields(&before, section).iter().enumerate() {
                set(&mut cfg, section, i, &f.value)
                    .unwrap_or_else(|e| panic!("{section}:{i} {}: {e}", f.label));
            }
            assert_eq!(cfg, before, "section {section} drifted");
            // And the table itself is stable.
            assert_eq!(fields(&cfg, section), fields(&before, section));
        }
    }

    #[test]
    fn navigation_moves_rows_and_switches_focus() {
        let (mut s, mut cfg) = (Settings::new(), Config::default());
        s.on_key(c('j'), &mut cfg);
        assert_eq!(s.row, 1);
        s.on_key(c('k'), &mut cfg);
        assert_eq!(s.row, 0);
        s.on_key(k(KeyCode::Tab), &mut cfg);
        assert_eq!(s.focus, Focus::Sections);
        s.on_key(k(KeyCode::Down), &mut cfg);
        assert_eq!(s.section, 1);
        s.on_key(k(KeyCode::Right), &mut cfg);
        assert_eq!(s.focus, Focus::Fields);
        // Off the ends is a no-op, not a panic.
        s.on_key(k(KeyCode::Up), &mut cfg);
        assert_eq!(s.row, 0);
    }

    #[test]
    fn mouse_selects_sections_and_rows() {
        let (mut s, mut cfg) = (Settings::new(), Config::default());
        let area = Rect::new(0, 0, 80, 24);
        let g = geometry(area);

        s.on_mouse(click(g.sections.x + 1, g.sections.y + 2), area, &mut cfg);
        assert_eq!(s.section, 2);

        s.on_mouse(click(g.fields.x + 3, g.fields.y + 3), area, &mut cfg);
        assert_eq!(s.row, 3);
        assert_eq!(fields(&cfg, 2)[s.row].label, "right");
    }

    #[test]
    fn clicking_a_bool_row_toggles_it() {
        let (mut s, mut cfg) = (Settings::new(), Config::default());
        let area = Rect::new(0, 0, 80, 24);
        let g = geometry(area);
        let i = fields(&cfg, 0)
            .iter()
            .position(|f| f.label == "mouse")
            .unwrap();
        let out = s.on_mouse(click(g.fields.x + 1, g.fields.y + i as u16), area, &mut cfg);
        assert_eq!(out, Outcome::Apply);
        assert!(!cfg.general.mouse);
    }

    #[test]
    fn wheel_scrolls_the_field_list() {
        let (mut s, mut cfg) = (Settings::new(), Config::default());
        let area = Rect::new(0, 0, 40, 8);
        s.section = KEYS;
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 20,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        s.on_mouse(wheel, area, &mut cfg);
        assert_eq!(s.scroll, 1);
        let g = geometry(area);
        let (rows, total) = (g.fields.h as usize, fields(&cfg, KEYS).len());
        // Row 0 is still selected, so the view snaps back to keep it visible.
        assert_eq!(s.eff_scroll(rows, total), 0);
        s.row = 10;
        assert_eq!(s.eff_scroll(rows, total), 11 - rows);
    }

    #[test]
    fn draw_survives_a_tiny_area() {
        let cfg = Config::default();
        let mut s = Settings::new();
        s.section = KEYS;
        s.row = 20;
        let mut buf = Buffer::empty(TRect::new(0, 0, 10, 5));
        s.draw(&mut buf, Rect::new(0, 0, 10, 5), &cfg);
        // Also: a panel larger than the buffer must clip, not panic.
        s.draw(&mut buf, Rect::new(2, 2, 40, 40), &cfg);
    }

    #[test]
    fn draw_renders_the_section_names() {
        let cfg = Config::default();
        let s = Settings::new();
        let mut buf = Buffer::empty(TRect::new(0, 0, 80, 24));
        s.draw(&mut buf, Rect::new(0, 0, 80, 24), &cfg);
        let text: String = (0..24)
            .map(|y| {
                (0..80)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        for name in SECTIONS {
            assert!(text.contains(name), "missing {name}\n{text}");
        }
        assert!(text.contains("scrollback"));
        assert!(text.contains("esc close"));
    }
}
