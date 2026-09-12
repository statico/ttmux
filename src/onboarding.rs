//! The first-run welcome: pick a keymap, then a line saying where to go next.
//!
//! Shown only when there is no config file. Choosing writes one, so it never
//! appears twice -- and every choice it offers is reachable afterwards from
//! the settings panel, which the second step says out loud.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};

use crate::action::Action;
use crate::config::{Config, KeysPreset};
use crate::layout::Rect;

/// The presets in the order they are offered, with the pitch for each.
const CHOICES: &[(KeysPreset, &str, &str, &str)] = &[
    (
        KeysPreset::Vim,
        "Modern",
        "ctrl+t",
        "vim's window keys: s/v to split, hjkl to move, HJKL to resize.",
    ),
    (
        KeysPreset::Tmux,
        "tmux",
        "ctrl+b",
        "What stock tmux does: \" and % to split, arrows to move.",
    ),
    (
        KeysPreset::Screen,
        "screen",
        "ctrl+a",
        "GNU screen's keys: | and S to split, tab to cycle panes.",
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Pick,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Still on screen.
    Continue,
    /// Finished; the app closes the overlay.
    Done,
    /// Finished, and the config wants writing to disk.
    Save,
}

pub struct Welcome {
    step: Step,
    sel: usize,
}

impl Welcome {
    pub fn new() -> Welcome {
        Welcome {
            step: Step::Pick,
            sel: 0,
        }
    }

    pub fn on_key(&mut self, ev: KeyEvent, cfg: &mut Config) -> Outcome {
        match self.step {
            Step::Pick => match ev.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.sel = self.sel.checked_sub(1).unwrap_or(CHOICES.len() - 1);
                    Outcome::Continue
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    self.sel = (self.sel + 1) % CHOICES.len();
                    Outcome::Continue
                }
                // The numbers are the fastest path and are drawn as such.
                KeyCode::Char(c @ '1'..='3') => {
                    self.sel = c as usize - '1' as usize;
                    self.choose(cfg)
                }
                KeyCode::Enter | KeyCode::Char(' ') => self.choose(cfg),
                // Escaping still writes a config: otherwise the picker comes
                // back next launch, which reads as the app not listening.
                KeyCode::Esc => self.choose(cfg),
                _ => Outcome::Continue,
            },
            Step::Ready => Outcome::Done,
        }
    }

    fn choose(&mut self, cfg: &mut Config) -> Outcome {
        cfg.general.keys_preset = CHOICES[self.sel].0;
        self.step = Step::Ready;
        Outcome::Save
    }

    pub fn draw(&self, buf: &mut Buffer, inner: Rect, cfg: &Config) {
        match self.step {
            Step::Pick => self.draw_pick(buf, inner, cfg),
            Step::Ready => draw_ready(buf, inner, cfg),
        }
    }

    /// The navigation line; the modal chrome draws it, so the panel body
    /// never spells the same keys out a second time.
    pub fn hint(&self) -> &'static str {
        match self.step {
            Step::Pick => "↑↓ or 1-3 choose  enter confirm",
            Step::Ready => "any key to start",
        }
    }

    pub fn title(&self) -> &'static str {
        match self.step {
            Step::Pick => "welcome to ttmux",
            Step::Ready => "you're set",
        }
    }

    fn draw_pick(&self, buf: &mut Buffer, inner: Rect, cfg: &Config) {
        let mut row = Rows::new(buf, inner, cfg);
        row.line("Pick a set of keyboard shortcuts.", row.plain);
        row.skip();
        for (i, (_, name, leader, blurb)) in CHOICES.iter().enumerate() {
            let on = i == self.sel;
            let mark = if on { "›" } else { " " };
            let style = if on {
                row.accent.add_modifier(Modifier::BOLD)
            } else {
                row.plain
            };
            row.line(&format!("{mark} {}. {name}  ({leader})", i + 1), style);
            row.line(&format!("     {blurb}"), row.dim);
        }
        row.skip();
        row.line(
            "Nothing here is permanent -- every key, colour and layout",
            row.dim,
        );
        row.line("option can be changed later in settings.", row.dim);
    }
}

impl Default for Welcome {
    fn default() -> Welcome {
        Welcome::new()
    }
}

fn draw_ready(buf: &mut Buffer, inner: Rect, cfg: &Config) {
    let mut row = Rows::new(buf, inner, cfg);
    row.line("You're set.", row.accent.add_modifier(Modifier::BOLD));
    row.skip();
    // Read the keys back out of the resolved keymap rather than hardcoding
    // them: the text has to match the preset that was just chosen.
    for (label, action) in [
        ("Help", Action::ToggleHelp),
        ("Settings", Action::ToggleSettings),
        ("Commands", Action::CommandPalette),
        ("Split", Action::Split(crate::action::Dir::Down)),
    ] {
        row.line(
            &format!("  {label:<10}  {}", key_for(cfg, action)),
            row.plain,
        );
    }
    row.skip();
    row.line("Enjoy.", row.plain);
}

/// The first binding for `action`, or a placeholder when it is unbound.
fn key_for(cfg: &Config, action: Action) -> String {
    let (map, _) = cfg.keymap();
    map.iter()
        .find(|(_, a)| **a == action)
        .map_or_else(|| "(unbound)".into(), |(b, _)| b.to_string())
}

/// A cursor that writes one clipped line at a time down a rect.
struct Rows<'a> {
    buf: &'a mut Buffer,
    rect: Rect,
    y: u16,
    plain: Style,
    dim: Style,
    accent: Style,
}

impl<'a> Rows<'a> {
    fn new(buf: &'a mut Buffer, rect: Rect, cfg: &Config) -> Rows<'a> {
        Rows {
            y: rect.y,
            rect,
            buf,
            plain: Style::default().fg(cfg.status.fg.into()),
            dim: Style::default()
                .fg(cfg.status.fg.into())
                .add_modifier(Modifier::DIM),
            accent: Style::default().fg(cfg.status.accent.into()),
        }
    }

    fn line(&mut self, text: &str, style: Style) {
        if self.y < self.rect.bottom() {
            self.buf
                .set_stringn(self.rect.x, self.y, text, self.rect.w as usize, style);
        }
        self.y += 1;
    }

    fn skip(&mut self) {
        self.y += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::layout::Rect as RRect;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn a_number_picks_that_preset_and_asks_for_a_save() {
        let mut cfg = Config::default();
        let mut w = Welcome::new();
        assert_eq!(w.on_key(key(KeyCode::Char('3')), &mut cfg), Outcome::Save);
        assert_eq!(cfg.general.keys_preset, KeysPreset::Screen);
        // The second step ends on any key.
        assert_eq!(w.on_key(key(KeyCode::Char('x')), &mut cfg), Outcome::Done);
    }

    #[test]
    fn the_selection_wraps_in_both_directions() {
        let mut cfg = Config::default();
        let mut w = Welcome::new();
        w.on_key(key(KeyCode::Up), &mut cfg);
        assert_eq!(w.sel, CHOICES.len() - 1);
        w.on_key(key(KeyCode::Down), &mut cfg);
        assert_eq!(w.sel, 0);
    }

    #[test]
    fn escaping_still_writes_a_config_so_it_does_not_come_back() {
        let mut cfg = Config::default();
        let mut w = Welcome::new();
        assert_eq!(w.on_key(key(KeyCode::Esc), &mut cfg), Outcome::Save);
    }

    #[test]
    fn the_closing_screen_names_the_keys_of_the_chosen_preset() {
        for (i, (preset, ..)) in CHOICES.iter().enumerate() {
            let mut cfg = Config::default();
            let mut w = Welcome::new();
            w.on_key(key(KeyCode::Char((b'1' + i as u8) as char)), &mut cfg);
            assert_eq!(cfg.general.keys_preset, *preset);

            let mut buf = Buffer::empty(RRect::new(0, 0, 60, 20));
            w.draw(&mut buf, Rect::new(0, 0, 60, 20), &cfg);
            let text: String = buf.content().iter().map(|c| c.symbol()).collect();
            let help = key_for(&cfg, Action::ToggleHelp);
            assert!(help.contains("ctrl+"), "{preset:?} has no help key: {help}");
            assert!(
                text.contains(&help),
                "{preset:?}: {help} missing from panel"
            );
        }
    }

    #[test]
    fn a_panel_too_short_for_the_text_clips_instead_of_panicking() {
        let cfg = Config::default();
        let w = Welcome::new();
        for (width, height) in [(0, 0), (1, 1), (4, 2), (200, 3)] {
            let mut buf = Buffer::empty(RRect::new(0, 0, width.max(1), height.max(1)));
            w.draw(&mut buf, Rect::new(0, 0, width, height), &cfg);
        }
    }
}
