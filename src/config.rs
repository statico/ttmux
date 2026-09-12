//! Configuration: TOML on disk, fully editable from inside the UI.
//!
//! Everything here must round-trip: the settings UI mutates a `Config` and
//! writes it straight back out, so serde field names double as UI labels.

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
    /// Terminals report `shift+a` as `A` with the shift bit set; we fold that
    /// into the bare character so `a` and `A` are distinct but unambiguous.
    pub fn from_event(ev: KeyEvent) -> Self {
        let mut mods = ev.modifiers;
        let code = match ev.code {
            KeyCode::Char(c) if c.is_ascii_uppercase() => {
                mods.insert(KeyModifiers::SHIFT);
                KeyCode::Char(c.to_ascii_lowercase())
            }
            other => other,
        };
        mods.remove(KeyModifiers::NONE);
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
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusPosition {
    Top,
    Bottom,
    Hidden,
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
    /// Milliseconds to wait for a second chord after the prefix.
    pub prefix_timeout_ms: u64,
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
            prefix_timeout_ms: 1500,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct StatusBar {
    pub position: StatusPosition,
    /// Widget names, left to right. See `status::WIDGETS`.
    pub left: Vec<String>,
    pub center: Vec<String>,
    pub right: Vec<String>,
    pub bg: Rgb,
    pub fg: Rgb,
    pub accent: Rgb,
    /// Separator drawn between widgets.
    pub separator: String,
    /// `strftime`-ish format for the `time` widget (%H %M %S %d %m %Y %a %b).
    pub time_format: String,
}

impl Default for StatusBar {
    fn default() -> Self {
        Self {
            position: StatusPosition::Bottom,
            left: vec!["session".into(), "mode".into()],
            center: vec!["tabs".into()],
            right: vec!["agents".into(), "time".into()],
            bg: rgb("#1a1b26"),
            fg: rgb("#a9b1d6"),
            accent: rgb("#7aa2f7"),
            separator: " │ ".into(),
            time_format: "%H:%M".into(),
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    pub general: General,
    pub appearance: Appearance,
    pub status: StatusBar,
    pub agents: Agents,
    /// Binding string -> action string. Both sides parse leniently.
    pub keys: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: General::default(),
            appearance: Appearance::default(),
            status: StatusBar::default(),
            agents: Agents::default(),
            keys: default_keys(),
        }
    }
}

/// The default keymap. Prefix is `ctrl+a`, but direct chords work too.
pub fn default_keys() -> BTreeMap<String, String> {
    let pairs = [
        // Direct, no prefix needed.
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
        // Prefixed. Leader is ctrl+t; the pane keys follow vim's window
        // commands, and the tmux spellings are kept as aliases so the old
        // muscle memory still lands.
        ("ctrl+t s", "split down"),
        ("ctrl+t v", "split right"),
        ("ctrl+t \"", "split down"),
        ("ctrl+t %", "split right"),
        ("ctrl+t h", "focus left"),
        ("ctrl+t j", "focus down"),
        ("ctrl+t k", "focus up"),
        ("ctrl+t l", "focus right"),
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
        ("ctrl+t t", "new-tab"),
        ("ctrl+t c", "new-tab"),
        ("ctrl+t n", "next-tab"),
        ("ctrl+t p", "prev-tab"),
        ("ctrl+t &", "close-tab"),
        ("ctrl+t shift+a", "rename-tab"),
        ("ctrl+t [", "scroll-up 10"),
        ("ctrl+t ?", "help"),
        ("ctrl+t ,", "settings"),
        ("ctrl+t r", "reload-config"),
        ("ctrl+t shift+q", "quit"),
    ];
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// `$TTMUX_CONFIG`, else `$XDG_CONFIG_HOME/ttmux/ttmux.toml`, else
/// `~/.config/ttmux/ttmux.toml`.
///
/// A terminal tool belongs in `~/.config` on macOS too — nobody edits
/// `~/Library/Application Support` by hand.
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
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Resolved keymap. Unparseable entries are reported, not fatal.
    pub fn keymap(&self) -> (BTreeMap<Binding, crate::action::Action>, Vec<String>) {
        let mut map = BTreeMap::new();
        let mut errors = vec![];
        for (k, v) in &self.keys {
            match (k.parse::<Binding>(), v.parse::<crate::action::Action>()) {
                (Ok(b), Ok(a)) => {
                    map.insert(b, a);
                }
                (Err(e), _) | (_, Err(e)) => errors.push(format!("{k} = {v:?}: {e}")),
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
    fn default_keymap_is_fully_valid() {
        let (map, errors) = Config::default().keymap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(map.len(), Config::default().keys.len());
        assert_eq!(
            Config::default().prefixes(),
            vec!["ctrl+t".parse().unwrap()]
        );
    }

    #[test]
    fn partial_config_fills_in_defaults() {
        let c: Config = toml::from_str("[general]\nmouse = false\n").unwrap();
        assert!(!c.general.mouse);
        assert_eq!(c.general.scrollback, 10_000);
        assert_eq!(c.appearance.border_style, BorderStyle::Curved);
    }

    #[test]
    fn saves_and_loads_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/ttmux.toml");
        let mut c = Config::default();
        c.general.scrollback = 42;
        c.save(&p).unwrap();
        assert_eq!(Config::load(&p).unwrap(), c);
        assert_eq!(
            Config::load(&dir.path().join("missing.toml")).unwrap(),
            Config::default()
        );
    }
}
