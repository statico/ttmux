//! Configuration: TOML on disk, fully editable from inside the UI.
//!
//! Everything here must round-trip: the settings UI mutates a `Config` and
//! writes it straight back out. The settings UI labels its rows with these
//! same kebab-case names, but spells them out by hand rather than deriving
//! them, so renaming a field here means renaming it there too;
//! `settings_ui::every_config_key_has_a_row` is what catches the miss.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

// ---------------------------------------------------------------- key chords

/// One keypress. `ctrl+a`, `alt+shift+left`, `f5`, `space`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl Chord {
    pub fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        Self { code, mods }
    }

    /// Normalise an incoming key event so it compares equal to a parsed chord.
    ///
    /// Terminals report `shift+a` as `A` with the shift bit set. Folding the
    /// case back out keeps one spelling for the chord, so a config that says
    /// `shift+a` matches whichever form the terminal sends.
    pub fn from_event(ev: KeyEvent) -> Self {
        let mut mods = ev.modifiers;
        let code = match ev.code {
            KeyCode::Char(c) if c.is_ascii_uppercase() => {
                mods.insert(KeyModifiers::SHIFT);
                KeyCode::Char(c.to_ascii_lowercase())
            }
            other => other,
        };
        Self { code, mods }
    }
}

// `KeyCode` is not `Ord`, but the settings UI wants a stable listing order, so
// order chords the way they are written.
impl Ord for Chord {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.mods.bits(), self.to_string()).cmp(&(other.mods.bits(), other.to_string()))
    }
}

impl PartialOrd for Chord {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (bit, name) in [
            (KeyModifiers::CONTROL, "ctrl"),
            (KeyModifiers::ALT, "alt"),
            (KeyModifiers::SHIFT, "shift"),
            (KeyModifiers::SUPER, "super"),
        ] {
            if self.mods.contains(bit) {
                write!(f, "{name}+")?;
            }
        }
        match self.code {
            KeyCode::Char(' ') => write!(f, "space"),
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::F(n) => write!(f, "f{n}"),
            KeyCode::Enter => write!(f, "enter"),
            KeyCode::Tab => write!(f, "tab"),
            KeyCode::BackTab => write!(f, "backtab"),
            KeyCode::Backspace => write!(f, "backspace"),
            KeyCode::Delete => write!(f, "delete"),
            KeyCode::Insert => write!(f, "insert"),
            KeyCode::Esc => write!(f, "esc"),
            KeyCode::Left => write!(f, "left"),
            KeyCode::Right => write!(f, "right"),
            KeyCode::Up => write!(f, "up"),
            KeyCode::Down => write!(f, "down"),
            KeyCode::Home => write!(f, "home"),
            KeyCode::End => write!(f, "end"),
            KeyCode::PageUp => write!(f, "pageup"),
            KeyCode::PageDown => write!(f, "pagedown"),
            other => write!(f, "{other:?}"),
        }
    }
}

impl FromStr for Chord {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut mods = KeyModifiers::empty();
        let lower = s.trim().to_ascii_lowercase();
        let mut parts: Vec<&str> = lower.split('+').collect();
        // A trailing `+` means the key itself is `+`: "ctrl++" -> ["ctrl", "", ""].
        let key = match parts.pop() {
            Some("") if !parts.is_empty() => {
                parts.pop();
                "+".to_string()
            }
            Some("") | None => return Err(format!("empty key: {s:?}")),
            Some(k) => k.to_string(),
        };
        for m in parts {
            mods.insert(match m {
                "ctrl" | "control" | "c" => KeyModifiers::CONTROL,
                "alt" | "opt" | "option" | "meta" | "m" => KeyModifiers::ALT,
                "shift" | "s" => KeyModifiers::SHIFT,
                "super" | "cmd" | "win" => KeyModifiers::SUPER,
                other => return Err(format!("unknown modifier: {other}")),
            });
        }
        let code = match key.as_str() {
            "space" => KeyCode::Char(' '),
            "enter" | "return" | "cr" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "backspace" | "bs" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "insert" | "ins" => KeyCode::Insert,
            "esc" | "escape" => KeyCode::Esc,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdn" => KeyCode::PageDown,
            f if f.starts_with('f') && f[1..].parse::<u8>().is_ok() => {
                KeyCode::F(f[1..].parse().unwrap())
            }
            c if c.chars().count() == 1 => KeyCode::Char(c.chars().next().unwrap()),
            other => return Err(format!("unknown key: {other}")),
        };
        Ok(Chord { code, mods })
    }
}

/// A binding is one or two chords: `ctrl+a d` is prefix-then-key, tmux style.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Binding(pub Vec<Chord>);

impl Binding {
    pub fn prefixed(&self) -> bool {
        self.0.len() > 1
    }
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let joined: Vec<String> = self.0.iter().map(|c| c.to_string()).collect();
        f.write_str(&joined.join(" "))
    }
}

impl FromStr for Binding {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let chords: Result<Vec<Chord>, String> =
            s.split_whitespace().map(Chord::from_str).collect();
        let chords = chords?;
        if chords.is_empty() {
            return Err("empty binding".into());
        }
        Ok(Binding(chords))
    }
}

impl Serialize for Binding {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Binding {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

// -------------------------------------------------------------------- colors

/// A `#rrggbb` (or named) colour that round-trips through TOML as a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub Color);

impl Default for Rgb {
    fn default() -> Self {
        Rgb(Color::Reset)
    }
}

impl From<Rgb> for Color {
    fn from(v: Rgb) -> Color {
        v.0
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Color::Rgb(r, g, b) => write!(f, "#{r:02x}{g:02x}{b:02x}"),
            Color::Indexed(i) => write!(f, "{i}"),
            Color::Reset => write!(f, "default"),
            other => write!(f, "{}", format!("{other:?}").to_ascii_lowercase()),
        }
    }
}

impl FromStr for Rgb {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if let Some(hex) = s.strip_prefix('#') {
            if hex.len() != 6 {
                return Err(format!("colour must be #rrggbb: {s}"));
            }
            let v = u32::from_str_radix(hex, 16).map_err(|e| e.to_string())?;
            return Ok(Rgb(Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)));
        }
        if let Ok(i) = s.parse::<u8>() {
            return Ok(Rgb(Color::Indexed(i)));
        }
        Ok(Rgb(match s.to_ascii_lowercase().as_str() {
            "default" | "reset" | "none" => Color::Reset,
            "black" => Color::Black,
            "red" => Color::Red,
            "green" => Color::Green,
            "yellow" => Color::Yellow,
            "blue" => Color::Blue,
            "magenta" => Color::Magenta,
            "cyan" => Color::Cyan,
            "gray" | "grey" => Color::Gray,
            "darkgray" | "darkgrey" => Color::DarkGray,
            "white" => Color::White,
            other => return Err(format!("unknown colour: {other}")),
        }))
    }
}

impl Serialize for Rgb {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

fn rgb(hex: &str) -> Rgb {
    hex.parse().expect("built-in colour literal")
}

// ------------------------------------------------------------------- config

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BorderStyle {
    Curved,
    Square,
    Heavy,
    Double,
    Dashed,
    /// One shared line between panes, like tmux: no box, no corners.
    Divider,
    None,
}

/// Background painting for a status row. `Flat` is a solid `bg`; the others
/// are painted by `status::draw`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BarEffect {
    #[default]
    Flat,
    Starfield,
    Gradient,
}

/// Which stock keymap the bindings start from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeysPreset {
    #[default]
    Vim,
    Tmux,
    Screen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TitlePosition {
    Top,
    Bottom,
    Hidden,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct General {
    /// Shell to launch. Empty means `$SHELL`, then `/bin/sh`.
    pub shell: String,
    /// Extra args passed to the shell.
    pub shell_args: Vec<String>,
    pub mouse: bool,
    /// Lines of scrollback kept per pane.
    pub scrollback: usize,
    /// Start in free (floating) mode instead of tiling.
    pub free_mode: bool,
    /// Focus follows the mouse pointer without a click.
    pub focus_follows_mouse: bool,
    /// Let panes draw inline images (kitty, iTerm2, sixel) through to the
    /// host terminal.
    pub passthrough_images: bool,
    /// Milliseconds to wait for a second chord after the prefix.
    pub prefix_timeout_ms: u64,
    /// Stock keymap that `keys` overrides sit on top of.
    pub keys_preset: KeysPreset,
    /// Show the which-key hint popup while waiting for the chord after the prefix.
    pub which_key: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            shell: String::new(),
            shell_args: vec![],
            mouse: true,
            scrollback: 10_000,
            free_mode: false,
            focus_follows_mouse: false,
            passthrough_images: true,
            prefix_timeout_ms: 1500,
            keys_preset: KeysPreset::default(),
            which_key: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Appearance {
    pub border_style: BorderStyle,
    pub border: Rgb,
    pub border_focused: Rgb,
    pub border_alert: Rgb,
    pub title_position: TitlePosition,
    /// Dim the contents of unfocused panes.
    pub dim_unfocused: bool,
    /// Blank columns/rows kept between tiled panes.
    pub gap: u16,
    /// Draw a drop shadow behind floating panes.
    pub float_shadow: bool,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            border_style: BorderStyle::Curved,
            border: rgb("#3b4261"),
            border_focused: rgb("#7aa2f7"),
            border_alert: rgb("#e0af68"),
            title_position: TitlePosition::Top,
            dim_unfocused: false,
            gap: 0,
            float_shadow: true,
        }
    }
}

/// One status row's contents. Widget names, left to right. See `status::WIDGETS`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Bar {
    pub enabled: bool,
    pub left: Vec<String>,
    pub center: Vec<String>,
    pub right: Vec<String>,
}

// Header and footer are the same type with *different* defaults, which serde
// cannot express: `#[serde(default)]` on `Bar` can only name one `Default`, so
// `[status.header] enabled = true` would silently blank the other three
// fields. Deserializing an all-optional twin and laying it over that row's own
// defaults is the only way to keep a partial table partial. Do not collapse
// this into a plain `#[serde(default)]` — `partial_status_fills_in_defaults`
// is the test that catches it.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct BarPatch {
    enabled: Option<bool>,
    left: Option<Vec<String>>,
    center: Option<Vec<String>>,
    right: Option<Vec<String>>,
}

fn patch<'de, D: Deserializer<'de>>(d: D, mut base: Bar) -> Result<Bar, D::Error> {
    let p = BarPatch::deserialize(d)?;
    base.enabled = p.enabled.unwrap_or(base.enabled);
    base.left = p.left.unwrap_or(base.left);
    base.center = p.center.unwrap_or(base.center);
    base.right = p.right.unwrap_or(base.right);
    Ok(base)
}

fn default_header() -> Bar {
    Bar {
        enabled: false,
        left: vec!["host".into()],
        center: vec!["tabs".into()],
        right: vec!["session".into()],
    }
}

fn default_footer() -> Bar {
    Bar {
        enabled: true,
        left: vec!["session".into(), "mode".into()],
        center: vec!["tabs".into()],
        right: vec!["agents".into(), "time".into()],
    }
}

fn de_header<'de, D: Deserializer<'de>>(d: D) -> Result<Bar, D::Error> {
    patch(d, default_header())
}

fn de_footer<'de, D: Deserializer<'de>>(d: D) -> Result<Bar, D::Error> {
    patch(d, default_footer())
}

/// A status widget that runs a shell command and shows what it prints.
///
/// Name it in a row's `left`, `center` or `right` to place it. The output is
/// one line; tmux's `#[fg=red,bold]` markup in it is honoured, so a script
/// written for tmux's `status-right` works unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct CustomWidget {
    /// Run with `sh -c`, so pipes, globs and `~` all work.
    pub command: String,
    /// Seconds between runs. 0 runs it once, at startup and at every reload.
    pub interval: u64,
}

impl Default for CustomWidget {
    fn default() -> Self {
        // The same default as tmux's `status-interval`: high enough that a
        // heavy script is not a problem for someone who forgets to set it.
        Self {
            command: String::new(),
            interval: 15,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct StatusBar {
    /// Drawn on the top row. Independent of `footer`: either, neither or both.
    #[serde(deserialize_with = "de_header")]
    pub header: Bar,
    /// Drawn on the bottom row.
    #[serde(deserialize_with = "de_footer")]
    pub footer: Bar,
    pub bg: Rgb,
    pub fg: Rgb,
    pub accent: Rgb,
    /// Separator drawn between widgets.
    pub separator: String,
    /// `strftime`-ish format for the `time` widget (%H %M %S %d %m %Y %a %b).
    pub time_format: String,
    pub effect: BarEffect,
    /// Widgets of your own, keyed by the name you put in a row.
    pub widgets: BTreeMap<String, CustomWidget>,
}

impl Default for StatusBar {
    fn default() -> Self {
        Self {
            header: default_header(),
            footer: default_footer(),
            bg: rgb("#1a1b26"),
            fg: rgb("#a9b1d6"),
            accent: rgb("#7aa2f7"),
            separator: " │ ".into(),
            time_format: "%H:%M".into(),
            effect: BarEffect::Flat,
            widgets: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Agents {
    /// Watch pane titles and output for coding-agent activity.
    pub enabled: bool,
    /// Ring the terminal bell when a pane starts waiting for input.
    pub bell_on_attention: bool,
    /// Case-insensitive substrings marking "the agent needs me".
    pub attention_patterns: Vec<String>,
    /// Substrings marking "the agent is busy".
    pub busy_patterns: Vec<String>,
    /// Substrings marking "the agent finished".
    pub done_patterns: Vec<String>,
}

impl Default for Agents {
    fn default() -> Self {
        Self {
            enabled: true,
            bell_on_attention: true,
            attention_patterns: [
                "waiting for input",
                "needs your input",
                "awaiting approval",
                "permission",
                "(y/n)",
                "[y/n]",
                "continue?",
                "?",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            busy_patterns: ["working", "thinking", "running", "esc to interrupt", "…"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            done_patterns: ["done", "complete", "finished", "✓"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    pub general: General,
    pub appearance: Appearance,
    pub status: StatusBar,
    pub agents: Agents,
    /// Overrides on top of `general.keys-preset`: binding string -> action
    /// string. An action of `""` or `"none"` unbinds the preset's binding.
    pub keys: BTreeMap<String, String>,
}

/// Chords that need no prefix. Shared by every preset.
fn direct_keys() -> Vec<(&'static str, &'static str)> {
    vec![
        ("ctrl+alt+right", "split right"),
        ("ctrl+alt+down", "split down"),
        ("ctrl+alt+w", "close-pane"),
        ("alt+left", "focus left"),
        ("alt+right", "focus right"),
        ("alt+up", "focus up"),
        ("alt+down", "focus down"),
        ("alt+shift+left", "resize left 2"),
        ("alt+shift+right", "resize right 2"),
        ("alt+shift+up", "resize up 1"),
        ("alt+shift+down", "resize down 1"),
        ("ctrl+alt+f", "toggle-layout-mode"),
        ("ctrl+alt+z", "toggle-zoom"),
        ("ctrl+alt+t", "new-tab"),
        ("ctrl+alt+,", "settings"),
        ("ctrl+alt+p", "command-palette"),
        ("ctrl+alt+n", "next-alert"),
        ("shift+pageup", "scroll-up 10"),
        ("shift+pagedown", "scroll-down 10"),
    ]
}

/// A stock keymap. Every preset keeps the direct chords and adds its own
/// prefixed bindings.
pub fn preset_keys(p: KeysPreset) -> BTreeMap<String, String> {
    let prefixed: &[(&str, &str)] = match p {
        // Leader is ctrl+t; the pane keys follow vim's window commands, with
        // the tmux spellings kept as aliases so old muscle memory still lands.
        KeysPreset::Vim => &[
            ("ctrl+t s", "split down"),
            ("ctrl+t v", "split right"),
            ("ctrl+t %", "split right"),
            ("ctrl+t h", "focus left"),
            ("ctrl+t j", "focus down"),
            ("ctrl+t k", "focus up"),
            ("ctrl+t l", "focus right"),
            ("ctrl+t ctrl+h", "focus left"),
            ("ctrl+t ctrl+j", "focus down"),
            ("ctrl+t ctrl+k", "focus up"),
            ("ctrl+t ctrl+l", "focus right"),
            ("ctrl+t shift+h", "resize left 2"),
            ("ctrl+t shift+j", "resize down 1"),
            ("ctrl+t shift+k", "resize up 1"),
            ("ctrl+t shift+l", "resize right 2"),
            ("ctrl+t w", "focus-next"),
            ("ctrl+t o", "toggle-zoom"),
            ("ctrl+t z", "toggle-zoom"),
            ("ctrl+t x", "close-pane"),
            ("ctrl+t q", "close-pane"),
            ("ctrl+t space", "next-preset"),
            ("ctrl+t f", "toggle-float"),
            ("ctrl+t t", "send-prefix"),
            ("ctrl+t c", "new-tab"),
            ("ctrl+t n", "next-tab"),
            ("ctrl+t p", "prev-tab"),
            ("ctrl+t &", "close-tab"),
            ("ctrl+t ctrl+t", "last-tab"),
            // tmux's break-pane and join-pane, on the same two keys.
            ("ctrl+t !", "break-pane"),
            ("ctrl+t @", "join-pane"),
            ("ctrl+t <", "move-tab left"),
            ("ctrl+t >", "move-tab right"),
            ("ctrl+t :", "command-line"),
            // tmux's `select-layout tiled`. There is no grid preset here, and
            // even-horizontal is the nearest "make it all equal".
            ("ctrl+t =", "set-preset even-horizontal"),
            ("ctrl+t shift+a", "rename-tab"),
            ("ctrl+t a", "rename-pane"),
            ("ctrl+t [", "scroll-up 10"),
            // tmux's choose-tree / choose-window; the palette is the nearest
            // chooser ttmux has.
            ("ctrl+t ;", "command-palette"),
            ("ctrl+t \"", "command-palette"),
            // `esc` is copy-mode in tmux; scrollback is the closest analogue.
            ("ctrl+t esc", "scroll-up 10"),
            ("ctrl+t ?", "help"),
            ("ctrl+t ,", "settings"),
            ("ctrl+t r", "reload-config"),
            ("ctrl+t shift+q", "quit"),
            // tmux is `prefix d`; `prefix C-d` is common enough in the wild
            // that both spellings are bound.
            ("ctrl+t d", "detach"),
            ("ctrl+t ctrl+d", "detach"),
        ],
        KeysPreset::Tmux => &[
            ("ctrl+b \"", "split down"),
            ("ctrl+b %", "split right"),
            ("ctrl+b o", "focus-next"),
            ("ctrl+b left", "focus left"),
            ("ctrl+b right", "focus right"),
            ("ctrl+b up", "focus up"),
            ("ctrl+b down", "focus down"),
            ("ctrl+b z", "toggle-zoom"),
            ("ctrl+b x", "close-pane"),
            ("ctrl+b c", "new-tab"),
            ("ctrl+b n", "next-tab"),
            ("ctrl+b p", "prev-tab"),
            ("ctrl+b &", "close-tab"),
            ("ctrl+b ,", "rename-tab"),
            ("ctrl+b .", "rename-pane"),
            ("ctrl+b !", "break-pane"),
            ("ctrl+b :", "command-line"),
            ("ctrl+b ?", "help"),
            ("ctrl+b space", "next-preset"),
            ("ctrl+b [", "scroll-up 10"),
            ("ctrl+b d", "detach"),
            ("ctrl+b ctrl+b", "send-prefix"),
        ],
        KeysPreset::Screen => &[
            ("ctrl+a |", "split right"),
            ("ctrl+a shift+s", "split down"),
            ("ctrl+a tab", "focus-next"),
            ("ctrl+a c", "new-tab"),
            ("ctrl+a n", "next-tab"),
            ("ctrl+a p", "prev-tab"),
            ("ctrl+a shift+a", "rename-tab"),
            // screen's own `C-a a`, which a nested session or readline's
            // start-of-line needs.
            ("ctrl+a a", "send-prefix"),
            ("ctrl+a .", "rename-pane"),
            ("ctrl+a shift+k", "close-pane"),
            ("ctrl+a :", "command-line"),
            ("ctrl+a ?", "help"),
            ("ctrl+a esc", "scroll-up 10"),
            ("ctrl+a d", "detach"),
        ],
    };
    direct_keys()
        .into_iter()
        .chain(prefixed.iter().copied())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// `$TTMUX_CONFIG`, else `$XDG_CONFIG_HOME/ttmux/ttmux.toml`, else
/// `~/.config/ttmux/ttmux.toml`.
///
/// A terminal tool belongs in `~/.config` on macOS too — nobody edits
/// `~/Library/Application Support` by hand.
/// Write the config readable only by its owner. Widget commands live in it
/// and ttmux runs them with `sh -c`, so who can edit the file is who can run
/// code as this user.
pub fn write_private(path: &Path, text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(text.as_bytes())?;
    Ok(())
}

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("TTMUX_CONFIG") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("ttmux").join("ttmux.toml")
}

impl Config {
    /// Load from `path`, falling back to defaults when it does not exist.
    /// Load, and hand back the exact text that was parsed. The hot reload
    /// compares against that text rather than an mtime, so an edit written
    /// while ttmux was starting is still noticed.
    pub fn load_stamped(path: &Path) -> (anyhow::Result<Config>, String) {
        match std::fs::read_to_string(path) {
            Ok(text) => (toml::from_str(&text).map_err(anyhow::Error::from), text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                (Ok(Config::default()), String::new())
            }
            Err(e) => (Err(e.into()), String::new()),
        }
    }

    pub fn load(path: &Path) -> anyhow::Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        write_private(path, &toml::to_string_pretty(self)?)
    }

    /// The preset resolved, then `keys` applied on top. An override whose
    /// action is `""` or `"none"` unbinds instead of binding. Unparseable
    /// entries are reported, not fatal.
    pub fn keymap(&self) -> (BTreeMap<Binding, crate::action::Action>, Vec<String>) {
        let mut map = BTreeMap::new();
        let mut errors = vec![];
        let preset = preset_keys(self.general.keys_preset);
        for (k, v) in preset.iter().chain(self.keys.iter()) {
            let b = match k.parse::<Binding>() {
                Ok(b) => b,
                Err(e) => {
                    errors.push(format!("{k} = {v:?}: {e}"));
                    continue;
                }
            };
            // Unbind. Keyed by the parsed binding, so spelling need not match.
            if v.trim().is_empty() || v.trim() == "none" {
                map.remove(&b);
                continue;
            }
            match v.parse::<crate::action::Action>() {
                Ok(a) => {
                    map.insert(b, a);
                }
                Err(e) => errors.push(format!("{k} = {v:?}: {e}")),
            }
        }
        (map, errors)
    }

    /// Chords that begin a two-step binding, e.g. `ctrl+a`.
    pub fn prefixes(&self) -> Vec<Chord> {
        let (map, _) = self.keymap();
        let mut v: Vec<Chord> = map
            .keys()
            .filter(|b| b.prefixed())
            .map(|b| b.0[0])
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_widgets_round_trip_through_toml() {
        let mut cfg = Config::default();
        cfg.status.widgets.insert(
            "checks".into(),
            CustomWidget {
                command: "test -x ~/bin/checks-tmux && ~/bin/checks-tmux".into(),
                interval: 60,
            },
        );
        cfg.status.footer.right = vec!["checks".into(), "time".into()];
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn a_widget_with_only_a_command_gets_the_default_interval() {
        let cfg: Config = toml::from_str(
            r#"
            [status.widgets.now]
            command = "date"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.status.widgets["now"].command, "date");
        assert_eq!(cfg.status.widgets["now"].interval, 15);
    }

    #[test]
    fn chords_round_trip() {
        for s in [
            "ctrl+a",
            "alt+shift+left",
            "f5",
            "space",
            "esc",
            "a",
            "ctrl++",
            "super+k",
        ] {
            let c: Chord = s.parse().unwrap();
            assert_eq!(c.to_string().parse::<Chord>().unwrap(), c, "{s}");
        }
        assert!("ctrl+nonsense".parse::<Chord>().is_err());
        assert!("hyper+a".parse::<Chord>().is_err());
    }

    #[test]
    fn uppercase_events_fold_into_shift() {
        let ev = KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT);
        assert_eq!(Chord::from_event(ev), "shift+a".parse().unwrap());
    }

    #[test]
    fn colours_round_trip() {
        for s in ["#7aa2f7", "default", "red", "42"] {
            let c: Rgb = s.parse().unwrap();
            assert_eq!(c.to_string(), s);
        }
        assert!("#xyz".parse::<Rgb>().is_err());
    }

    #[test]
    fn default_config_round_trips_through_toml() {
        let c = Config::default();
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn every_preset_is_valid_with_one_leader() {
        for (p, leader) in [
            (KeysPreset::Vim, "ctrl+t"),
            (KeysPreset::Tmux, "ctrl+b"),
            (KeysPreset::Screen, "ctrl+a"),
        ] {
            let mut c = Config::default();
            c.general.keys_preset = p;
            let (map, errors) = c.keymap();
            assert!(errors.is_empty(), "{p:?}: {errors:?}");
            assert_eq!(map.len(), preset_keys(p).len(), "{p:?}");
            assert_eq!(c.prefixes(), vec![leader.parse().unwrap()], "{p:?}");
        }
    }

    #[test]
    fn overrides_replace_and_unbind() {
        let mut c = Config::default();
        c.keys.insert("ctrl+t z".into(), "quit".into());
        c.keys.insert("ctrl+t x".into(), "".into());
        c.keys.insert("ctrl+t q".into(), "none".into());
        let (map, errors) = c.keymap();
        assert!(errors.is_empty(), "{errors:?}");
        let b = |s: &str| s.parse::<Binding>().unwrap();
        assert_eq!(map[&b("ctrl+t z")], crate::action::Action::Quit);
        assert!(!map.contains_key(&b("ctrl+t x")));
        assert!(!map.contains_key(&b("ctrl+t q")));
    }

    #[test]
    fn default_keys_are_empty_but_the_keymap_is_not() {
        let c = Config::default();
        assert!(c.keys.is_empty());
        assert!(!c.keymap().0.is_empty());
    }

    #[test]
    fn partial_config_fills_in_defaults() {
        let c: Config = toml::from_str("[general]\nmouse = false\n").unwrap();
        assert!(!c.general.mouse);
        assert_eq!(c.general.scrollback, 10_000);
        assert_eq!(c.appearance.border_style, BorderStyle::Curved);
    }

    #[test]
    fn partial_status_fills_in_defaults() {
        let c: Config = toml::from_str("[status.header]\nenabled = true\n").unwrap();
        assert!(c.status.header.enabled);
        assert_eq!(c.status.header.center, vec!["tabs".to_string()]);
        assert_eq!(c.status.footer, StatusBar::default().footer);
        assert_eq!(c.status.effect, BarEffect::Flat);
        assert_eq!(c.status.time_format, "%H:%M");
    }

    #[test]
    fn save_creates_missing_directories_and_load_reads_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/ttmux.toml");
        let mut c = Config::default();
        c.general.scrollback = 42;
        c.save(&p).unwrap();
        assert_eq!(Config::load(&p).unwrap(), c);
    }

    #[test]
    fn load_of_a_missing_file_is_the_default_config() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Config::load(&dir.path().join("missing.toml")).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn save_then_load_preserves_every_non_default_field() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ttmux.toml");
        let mut c = Config::default();
        c.general.shell = "/bin/zsh".into();
        c.general.shell_args = vec!["-l".into()];
        c.general.mouse = false;
        c.general.scrollback = 7;
        c.general.free_mode = true;
        c.general.focus_follows_mouse = true;
        c.general.passthrough_images = false;
        c.general.prefix_timeout_ms = 250;
        c.general.keys_preset = KeysPreset::Screen;
        c.appearance.border_style = BorderStyle::None;
        c.appearance.border = rgb("#010203");
        c.appearance.border_focused = Rgb(Color::Reset);
        c.appearance.border_alert = rgb("#0f0f0f");
        c.appearance.title_position = TitlePosition::Hidden;
        c.appearance.dim_unfocused = true;
        c.appearance.gap = 3;
        c.appearance.float_shadow = false;
        c.status.header = Bar {
            enabled: true,
            left: vec!["time".into()],
            center: vec![],
            right: vec!["host".into(), "session".into()],
        };
        c.status.footer = Bar::default();
        c.status.bg = rgb("#000000");
        c.status.fg = rgb("#ffffff");
        c.status.accent = Rgb(Color::Indexed(9));
        c.status.separator = " ~ ".into();
        c.status.time_format = "%Y-%m-%d".into();
        c.status.effect = BarEffect::Starfield;
        c.agents.enabled = false;
        c.agents.bell_on_attention = false;
        c.agents.attention_patterns = vec!["?".into()];
        c.agents.busy_patterns = vec![];
        c.agents.done_patterns = vec!["done".into()];
        c.keys.insert("ctrl+t z".into(), "quit".into());
        c.keys.insert("ctrl+t x".into(), "none".into());

        c.save(&p).unwrap();
        assert_eq!(Config::load(&p).unwrap(), c);
    }

    // An empty `keys` table is the default, and a round trip must not turn it
    // into a missing one that then re-inherits anything.
    #[test]
    fn save_then_load_preserves_an_empty_keys_map() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ttmux.toml");
        let mut c = Config::default();
        c.status.effect = BarEffect::Gradient;
        c.general.keys_preset = KeysPreset::Tmux;
        c.save(&p).unwrap();
        let back = Config::load(&p).unwrap();
        assert!(back.keys.is_empty());
        assert_eq!(back, c);
    }
}
