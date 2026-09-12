//! The `<prefix> :` command line: type a scripting command, with completion.
//!
//! The text buffer is a [`LineEdit`], so the readline keys are the same here
//! as in every other field. What this adds is knowledge of
//! [`crate::script::COMMANDS`]: Tab completes a command name or a flag of the
//! command already named, and a hint line under the input spells out the
//! usage of that command while it is being typed, so nothing has to be
//! guessed.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};

use crate::config::Config;
use crate::layout::Rect;
use crate::line_edit::{LineEdit, CARET};
use crate::render::put_cell;
use crate::script;

/// What the app must do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Stay open.
    Stay,
    /// Close without running anything.
    Cancel,
    /// Close and run this line.
    Run(String),
}

#[derive(Debug, Default)]
pub struct CmdLine {
    input: LineEdit,
    /// The candidates the last Tab could not choose between, for display.
    candidates: Vec<String>,
    /// Lines run this session, oldest first.
    history: Vec<String>,
    /// Where Up/Down is in `history`; `None` means at the live line.
    browsing: Option<usize>,
}

impl CmdLine {
    pub fn new() -> CmdLine {
        CmdLine::default()
    }

    /// The line as typed, for the app to hand to `script::parse`.
    pub fn line(&self) -> &str {
        self.input.text()
    }

    pub fn key(&mut self, ev: KeyEvent) -> Outcome {
        match ev.code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Enter => {
                let line = self.input.text().trim().to_string();
                if line.is_empty() {
                    return Outcome::Stay;
                }
                self.history.push(line.clone());
                return Outcome::Run(line);
            }
            KeyCode::Tab => {
                self.complete();
                return Outcome::Stay;
            }
            KeyCode::Up | KeyCode::Down if !self.history.is_empty() => {
                self.walk(ev.code == KeyCode::Up);
                return Outcome::Stay;
            }
            _ => {}
        }
        if self.input.key(ev) {
            self.candidates.clear();
        }
        Outcome::Stay
    }

    /// Step through the history: Up goes back, Down forward and then off the
    /// end to the empty line again.
    fn walk(&mut self, back: bool) {
        let last = self.history.len() - 1;
        self.browsing = match (self.browsing, back) {
            (None, true) => Some(last),
            (None, false) => None,
            (Some(i), true) => Some(i.saturating_sub(1)),
            (Some(i), false) if i < last => Some(i + 1),
            (Some(_), false) => None,
        };
        let text = self.browsing.map_or("", |i| self.history[i].as_str());
        self.input = LineEdit::new(text);
        self.candidates.clear();
    }

    /// The word Tab would complete: what sits at the end of the line, and the
    /// byte offset it starts at.
    fn word(&self) -> (&str, usize, usize) {
        let text = self.input.text();
        let words: Vec<&str> = text.split_whitespace().collect();
        let fresh = text.is_empty() || text.ends_with(char::is_whitespace);
        let cur = if fresh { "" } else { *words.last().unwrap() };
        let index = if fresh { words.len() } else { words.len() - 1 };
        (cur, index, text.len() - cur.len())
    }

    /// What the word at the end of the line could become.
    fn matches(&self) -> Vec<String> {
        let (cur, index, _) = self.word();
        if index == 0 {
            return script::COMMANDS
                .iter()
                .filter(|c| c.name.starts_with(cur))
                .map(|c| c.name.to_string())
                .collect();
        }
        let verb = self.input.text().split_whitespace().next().unwrap_or("");
        let Some(spec) = script::spec(verb) else {
            return Vec::new();
        };
        spec.flags
            .iter()
            .flat_map(|f| f.short.into_iter().chain(std::iter::once(f.long)))
            .filter(|f| f.starts_with(cur))
            .map(String::from)
            .collect()
    }

    /// Tab: a lone match lands in the line, several fill in as far as they
    /// agree and are then listed.
    fn complete(&mut self) {
        let hits = self.matches();
        let (cur, _, at) = self.word();
        let fill = match hits.as_slice() {
            [] => return,
            [one] => format!("{one} "),
            many => common_prefix(many),
        };
        if fill.len() > cur.len() {
            let line = format!("{}{fill}", &self.input.text()[..at]);
            self.input = LineEdit::new(line);
        }
        self.candidates = if hits.len() > 1 { hits } else { Vec::new() };
    }

    /// The line under the input: the usage of the command being typed, or,
    /// before one is named, what the first word could still become.
    fn hint(&self) -> String {
        if !self.candidates.is_empty() {
            return self.candidates.join("  ");
        }
        let text = self.input.text();
        let verb = text.split_whitespace().next().unwrap_or("");
        if let Some(spec) = script::spec(verb) {
            return format!("{}  --  {}", spec.usage(), spec.about);
        }
        if script::is_command(verb) {
            return format!("{verb}: an alias, Enter runs it");
        }
        let names = self.matches();
        match names.len() {
            0 if verb.is_empty() => "type a command, Tab completes".into(),
            0 => format!("no command starts with {verb:?}"),
            _ => names.join("  "),
        }
    }

    /// The prompt on the bottom row of `area`, its hint on the row above.
    pub fn draw(&self, buf: &mut Buffer, area: Rect, cfg: &Config) {
        if area.w == 0 || area.h == 0 {
            return;
        }
        let style = Style::default()
            .fg(cfg.status.fg.into())
            .bg(cfg.status.bg.into());
        let y = area.bottom() - 1;
        write_line(
            buf,
            area,
            y,
            &format!(":{}", self.input.with_caret(CARET)),
            style,
        );
        if area.h > 1 {
            let hint = style
                .fg(cfg.status.accent.into())
                .add_modifier(Modifier::ITALIC);
            write_line(buf, area, y - 1, &self.hint(), hint);
        }
    }
}

/// The longest start every candidate shares.
fn common_prefix(words: &[String]) -> String {
    let mut out = String::new();
    'outer: for (i, c) in words[0].chars().enumerate() {
        for w in &words[1..] {
            if w.chars().nth(i) != Some(c) {
                break 'outer;
            }
        }
        out.push(c);
    }
    out
}

/// One row of `area`, blanked and then filled as far as `text` and the width
/// allow. Anything outside `area` -- or outside the buffer -- is dropped.
fn write_line(buf: &mut Buffer, area: Rect, y: u16, text: &str, style: Style) {
    let mut chars = text.chars();
    for x in area.x..area.right() {
        let c = chars.next().unwrap_or(' ');
        let mut s = [0u8; 4];
        put_cell(buf, x, y, c.encode_utf8(&mut s), style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::layout::Rect as Area;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(s: &str) -> CmdLine {
        let mut c = CmdLine::new();
        for ch in s.chars() {
            c.key(key(KeyCode::Char(ch)));
        }
        c
    }

    fn tab(c: &mut CmdLine) {
        c.key(key(KeyCode::Tab));
    }

    #[test]
    fn tab_on_a_unique_prefix_completes_the_whole_command() {
        let mut c = typed("spl");
        tab(&mut c);
        assert_eq!(c.line(), "split-window ");
    }

    #[test]
    fn an_ambiguous_prefix_fills_in_what_the_candidates_share() {
        let mut c = typed("list-");
        tab(&mut c);
        assert_eq!(c.line(), "list-");
        assert!(c.candidates.len() > 1);
        assert!(c.candidates.iter().all(|n| n.starts_with("list-")));
    }

    #[test]
    fn an_ambiguous_prefix_offers_its_candidates_in_the_hint() {
        let mut c = typed("s");
        tab(&mut c);
        assert!(c.candidates.contains(&"split-window".to_string()));
        assert!(c.candidates.contains(&"send-keys".to_string()));
        assert!(c.hint().contains("send-keys"));
    }

    #[test]
    fn tab_after_a_dash_offers_the_flags_of_the_command_named() {
        let mut c = typed("split-window -");
        tab(&mut c);
        assert!(c.candidates.contains(&"-h".to_string()));
        assert!(c.candidates.contains(&"--vertical".to_string()));
        // A flag of some other command has no business here.
        assert!(!c.candidates.contains(&"--literal".to_string()));
    }

    #[test]
    fn a_unique_flag_prefix_completes_in_place() {
        let mut c = typed("split-window --hor");
        tab(&mut c);
        assert_eq!(c.line(), "split-window --horizontal ");
    }

    #[test]
    fn a_flag_of_an_unknown_command_completes_to_nothing() {
        let mut c = typed("frobnicate --");
        tab(&mut c);
        assert_eq!(c.line(), "frobnicate --");
        assert!(c.candidates.is_empty());
    }

    #[test]
    fn the_hint_shows_the_usage_and_the_about_once_the_command_is_named() {
        let c = typed("split-window ");
        let spec = script::spec("split-window").unwrap();
        let hint = c.hint();
        assert!(hint.contains(&spec.usage()));
        assert!(hint.contains(spec.about));
    }

    #[test]
    fn the_hint_lists_the_command_names_still_matching_while_typing() {
        let hint = typed("kill-").hint();
        assert!(hint.contains("kill-pane"));
        assert!(!hint.contains("split-window"));
    }

    #[test]
    fn the_hint_says_so_when_nothing_matches() {
        assert!(typed("zzz").hint().contains("no command"));
    }

    #[test]
    fn the_hint_names_an_alias_as_one() {
        assert!(typed("lsp ").hint().contains("alias"));
    }

    #[test]
    fn enter_on_an_empty_line_does_not_run_anything() {
        let mut c = CmdLine::new();
        assert_eq!(c.key(key(KeyCode::Enter)), Outcome::Stay);
        let mut c = typed("   ");
        assert_eq!(c.key(key(KeyCode::Enter)), Outcome::Stay);
        assert!(c.history.is_empty());
    }

    #[test]
    fn enter_runs_the_trimmed_line() {
        let mut c = typed(" kill-pane ");
        assert_eq!(c.key(key(KeyCode::Enter)), Outcome::Run("kill-pane".into()));
    }

    #[test]
    fn esc_cancels() {
        let mut c = typed("kill-pane");
        assert_eq!(c.key(key(KeyCode::Esc)), Outcome::Cancel);
    }

    #[test]
    fn up_walks_back_through_the_lines_run_and_down_comes_back() {
        let mut c = CmdLine::new();
        for line in ["kill-pane", "new-window"] {
            for ch in line.chars() {
                c.key(key(KeyCode::Char(ch)));
            }
            c.key(key(KeyCode::Enter));
            c.input = LineEdit::default();
        }
        c.key(key(KeyCode::Up));
        assert_eq!(c.line(), "new-window");
        c.key(key(KeyCode::Up));
        assert_eq!(c.line(), "kill-pane");
        c.key(key(KeyCode::Down));
        assert_eq!(c.line(), "new-window");
        c.key(key(KeyCode::Down));
        assert_eq!(c.line(), "");
    }

    #[test]
    fn up_on_an_empty_history_is_left_to_the_line_editor() {
        let mut c = CmdLine::new();
        assert_eq!(c.key(key(KeyCode::Up)), Outcome::Stay);
        assert_eq!(c.line(), "");
    }

    #[test]
    fn the_readline_keys_still_reach_the_field() {
        let mut c = typed("kill-pane");
        c.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(c.line(), "");
    }

    #[test]
    fn every_command_completes_from_its_own_first_three_letters() {
        for spec in script::COMMANDS {
            let start: String = spec.name.chars().take(3).collect();
            let mut c = typed(&start);
            tab(&mut c);
            let line = c.line().trim_end();
            assert!(
                spec.name.starts_with(line),
                "{} went to {line:?}",
                spec.name
            );
            assert!(
                line == spec.name || c.candidates.iter().any(|n| n == spec.name),
                "{} is not among the candidates for {start:?}",
                spec.name
            );
        }
    }

    #[test]
    fn drawing_a_tiny_command_line_neither_panics_nor_leaves_its_area() {
        let mut buf = Buffer::empty(Area::new(0, 0, 10, 3));
        let c = typed("split-window --horizontal");
        c.draw(&mut buf, Rect::new(0, 1, 10, 2), &Config::default());
        // The row above the area is untouched.
        let untouched = Buffer::empty(Area::new(0, 0, 1, 1))[(0, 0)].style();
        for x in 0..10 {
            assert_eq!(buf[(x, 0)].symbol(), " ");
            assert_eq!(buf[(x, 0)].style(), untouched);
        }
        assert_eq!(buf[(0, 2)].symbol(), ":");
    }

    #[test]
    fn drawing_a_full_size_command_line_puts_the_prompt_on_the_last_row() {
        let mut buf = Buffer::empty(Area::new(0, 0, 80, 24));
        let mut c = typed("spl");
        tab(&mut c);
        c.draw(&mut buf, Rect::new(0, 0, 80, 24), &Config::default());
        let row: String = (0..80).map(|x| buf[(x, 23)].symbol()).collect();
        assert!(row.starts_with(":split-window"));
        let hint: String = (0..80).map(|x| buf[(x, 22)].symbol()).collect();
        assert!(hint.contains("split a pane in two"));
    }

    #[test]
    fn drawing_into_a_zero_sized_area_does_nothing() {
        let mut buf = Buffer::empty(Area::new(0, 0, 10, 3));
        CmdLine::new().draw(&mut buf, Rect::new(0, 0, 0, 0), &Config::default());
        CmdLine::new().draw(&mut buf, Rect::new(0, 0, 10, 0), &Config::default());
    }

    #[test]
    fn drawing_an_area_wider_than_the_buffer_clips_instead_of_panicking() {
        let mut buf = Buffer::empty(Area::new(0, 0, 10, 3));
        typed("send-keys").draw(&mut buf, Rect::new(5, 1, 40, 2), &Config::default());
    }
}
