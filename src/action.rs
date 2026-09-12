//! Every user-triggerable command. Config binds key strings to these.

use std::fmt;
use std::str::FromStr;

use crate::layout::Preset;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    pub fn as_str(self) -> &'static str {
        match self {
            Dir::Left => "left",
            Dir::Right => "right",
            Dir::Up => "up",
            Dir::Down => "down",
        }
    }
}

impl FromStr for Dir {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "left" | "h" => Dir::Left,
            "right" | "l" => Dir::Right,
            "up" | "k" => Dir::Up,
            "down" | "j" => Dir::Down,
            _ => return Err(format!("unknown direction: {s}")),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Split the focused pane.
    Split(Dir),
    /// Close the focused pane (asks the child to exit).
    ClosePane,
    /// Move focus in a direction.
    Focus(Dir),
    /// Cycle focus to the next/previous pane.
    FocusNext,
    FocusPrev,
    /// Grow the focused pane towards a direction.
    Resize(Dir, u16),
    /// Move the focused floating pane (free mode only).
    MovePane(Dir, u16),
    /// Swap focused pane with the next one (tiling only).
    SwapNext,
    /// Toggle between tiling and free (floating) layout.
    ToggleLayoutMode,
    /// Cycle tiling presets: even-h, even-v, main-v, main-h.
    NextPreset,
    /// Jump straight to one tiling preset.
    SetPreset(Preset),
    /// Zoom (maximise) the focused pane.
    ToggleZoom,
    /// Toggle floating for the focused pane (free mode).
    ToggleFloat,
    /// Tabs ("windows" in tmux speak).
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    SelectTab(usize),
    /// Back to the tab you were on before this one (tmux's `last-window`).
    LastTab,
    RenameTab,
    /// Scrollback.
    ScrollUp(usize),
    ScrollDown(usize),
    ScrollTop,
    ScrollBottom,
    /// Overlays.
    ToggleSettings,
    ToggleHelp,
    CommandPalette,
    /// Jump to the next pane flagged by an agent alert.
    NextAlert,
    /// Reload the config file from disk.
    ReloadConfig,
    /// Leave ttmux (kills children).
    Quit,
    /// Leave this view but keep the session and its panes running.
    Detach,
    /// Send the leader chord itself to the pane, tmux's `send-prefix`.
    SendPrefix,
    /// Do nothing (useful for unbinding).
    Nop,
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use Action::*;
        match self {
            Split(d) => write!(f, "split {}", d.as_str()),
            ClosePane => write!(f, "close-pane"),
            Focus(d) => write!(f, "focus {}", d.as_str()),
            FocusNext => write!(f, "focus-next"),
            FocusPrev => write!(f, "focus-prev"),
            Resize(d, n) => write!(f, "resize {} {}", d.as_str(), n),
            MovePane(d, n) => write!(f, "move {} {}", d.as_str(), n),
            SwapNext => write!(f, "swap-next"),
            ToggleLayoutMode => write!(f, "toggle-layout-mode"),
            NextPreset => write!(f, "next-preset"),
            SetPreset(p) => write!(f, "set-preset {}", p.as_str()),
            ToggleZoom => write!(f, "toggle-zoom"),
            ToggleFloat => write!(f, "toggle-float"),
            NewTab => write!(f, "new-tab"),
            CloseTab => write!(f, "close-tab"),
            NextTab => write!(f, "next-tab"),
            PrevTab => write!(f, "prev-tab"),
            SelectTab(i) => write!(f, "select-tab {i}"),
            LastTab => write!(f, "last-tab"),
            RenameTab => write!(f, "rename-tab"),
            ScrollUp(n) => write!(f, "scroll-up {n}"),
            ScrollDown(n) => write!(f, "scroll-down {n}"),
            ScrollTop => write!(f, "scroll-top"),
            ScrollBottom => write!(f, "scroll-bottom"),
            ToggleSettings => write!(f, "settings"),
            ToggleHelp => write!(f, "help"),
            CommandPalette => write!(f, "command-palette"),
            NextAlert => write!(f, "next-alert"),
            ReloadConfig => write!(f, "reload-config"),
            Quit => write!(f, "quit"),
            Detach => write!(f, "detach"),
            SendPrefix => write!(f, "send-prefix"),
            Nop => write!(f, "nop"),
        }
    }
}

impl FromStr for Action {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use Action::*;
        let parts: Vec<&str> = s.split_whitespace().collect();
        let head = parts.first().copied().unwrap_or("");
        let arg = parts.get(1).copied();
        let num = |d: u16| arg.and_then(|a| a.parse().ok()).unwrap_or(d);
        let dir = || -> Result<Dir, String> {
            arg.ok_or_else(|| format!("`{head}` needs a direction"))?
                .parse()
        };
        // Optional numeric argument in third position: `verb <dir> [n]`.
        let dir_n = |d: u16| parts.get(2).and_then(|a| a.parse().ok()).unwrap_or(d);
        Ok(match head {
            "split" => Split(dir()?),
            "close-pane" => ClosePane,
            "focus" => Focus(dir()?),
            "focus-next" => FocusNext,
            "focus-prev" => FocusPrev,
            "resize" => {
                let d = dir()?;
                Resize(d, dir_n(2))
            }
            "move" => {
                let d = dir()?;
                MovePane(d, dir_n(1))
            }
            "swap-next" => SwapNext,
            "toggle-layout-mode" => ToggleLayoutMode,
            "next-preset" => NextPreset,
            "set-preset" => SetPreset(arg.ok_or("`set-preset` needs a name")?.parse()?),
            "toggle-zoom" => ToggleZoom,
            "toggle-float" => ToggleFloat,
            "new-tab" => NewTab,
            "close-tab" => CloseTab,
            "next-tab" => NextTab,
            "prev-tab" => PrevTab,
            "select-tab" => SelectTab(arg.and_then(|a| a.parse().ok()).unwrap_or(1)),
            "last-tab" => LastTab,
            "rename-tab" => RenameTab,
            "scroll-up" => ScrollUp(num(1) as usize),
            "scroll-down" => ScrollDown(num(1) as usize),
            "scroll-top" => ScrollTop,
            "scroll-bottom" => ScrollBottom,
            "settings" => ToggleSettings,
            "help" => ToggleHelp,
            "command-palette" => CommandPalette,
            "next-alert" => NextAlert,
            "reload-config" => ReloadConfig,
            "quit" => Quit,
            "detach" => Detach,
            "send-prefix" => SendPrefix,
            "nop" | "" => Nop,
            other => return Err(format!("unknown action: {other}")),
        })
    }
}

/// Every action, for the command palette and the settings UI.
pub const ALL_ACTIONS: &[Action] = &[
    Action::Split(Dir::Right),
    Action::Split(Dir::Down),
    Action::ClosePane,
    Action::Focus(Dir::Left),
    Action::Focus(Dir::Right),
    Action::Focus(Dir::Up),
    Action::Focus(Dir::Down),
    Action::FocusNext,
    Action::FocusPrev,
    Action::SwapNext,
    Action::ToggleLayoutMode,
    Action::NextPreset,
    Action::SetPreset(Preset::EvenHorizontal),
    Action::SetPreset(Preset::EvenVertical),
    Action::SetPreset(Preset::MainVertical),
    Action::SetPreset(Preset::MainHorizontal),
    Action::ToggleZoom,
    Action::ToggleFloat,
    Action::NewTab,
    Action::CloseTab,
    Action::NextTab,
    Action::PrevTab,
    Action::LastTab,
    Action::RenameTab,
    Action::ScrollTop,
    Action::ScrollBottom,
    Action::ToggleSettings,
    Action::ToggleHelp,
    Action::CommandPalette,
    Action::NextAlert,
    Action::ReloadConfig,
    Action::SendPrefix,
    Action::Quit,
    Action::Detach,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_strings() {
        for a in ALL_ACTIONS {
            let s = a.to_string();
            assert_eq!(
                &Action::from_str(&s).unwrap(),
                a,
                "round trip failed for {s}"
            );
        }
    }

    #[test]
    fn parses_arguments() {
        assert_eq!("resize left 5".parse(), Ok(Action::Resize(Dir::Left, 5)));
        assert_eq!("resize left".parse(), Ok(Action::Resize(Dir::Left, 2)));
        assert_eq!("select-tab 3".parse(), Ok(Action::SelectTab(3)));
        assert_eq!(
            "set-preset main-v".parse(),
            Ok(Action::SetPreset(Preset::MainVertical))
        );
        assert!("set-preset".parse::<Action>().is_err());
        assert!("set-preset sideways".parse::<Action>().is_err());
        assert!("frobnicate".parse::<Action>().is_err());
        assert!("focus sideways".parse::<Action>().is_err());
    }
}
