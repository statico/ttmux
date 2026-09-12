//! The scripting API: what `ttmux send-keys` and its siblings mean.
//!
//! This module only parses. The effects live in `App::script`, because a
//! command is the same thing a key binding is: a change to the running app.
//!
//! The names follow tmux, so a script written for tmux mostly reads the same
//! here. A target is a pane id (`%3`, or plain `3`) or a window number
//! counted from 1, matching what `list-panes` and `list-windows` print.

use std::str::FromStr;

use anyhow::{bail, Result};
use crossterm::event::{KeyEvent, KeyModifiers};

use crate::action::{Action, Dir};
use crate::config::Chord;
use crate::layout::PaneId;

pub const USAGE: &str = "\
scripting commands (each takes an optional -t target):

    send-keys [-t PANE] [-l] KEY...   type into a pane (\"C-c\", \"Enter\", text)
    split-window [-h|-v] [-t PANE]    split the pane: -h side by side, -v stacked
    new-window                        open a tab
    select-pane -t PANE | -L|-R|-U|-D focus a pane
    select-window -t N | -n | -p      focus a window
    kill-pane [-t PANE]               close a pane
    kill-window [-t N]                close a window
    rename-window [-t N] NAME         name a window
    rename-pane [-t PANE] NAME        name a pane
    list-panes                        one line per pane of the current window
    list-windows                      one line per window
    display-message TEXT              show TEXT in the status bar
    run ACTION                        any key-binding action, e.g. \"toggle-zoom\"
";

/// One scripted command, already parsed and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// Anything the keymap can do, named as a binding names it.
    Run(Action),
    SendKeys {
        target: Option<PaneId>,
        keys: Vec<KeyEvent>,
    },
    SelectPane(PaneId),
    SelectWindow(usize),
    KillPane(Option<PaneId>),
    KillWindow(Option<usize>),
    RenameWindow {
        target: Option<usize>,
        name: String,
    },
    RenamePane {
        target: Option<PaneId>,
        name: String,
    },
    ListPanes,
    ListWindows,
    Display(String),
}

/// True for a verb this module handles, so the CLI can tell a scripting
/// command from a session command without parsing it twice.
pub fn is_command(verb: &str) -> bool {
    matches!(
        verb,
        "send-keys"
            | "split-window"
            | "new-window"
            | "select-pane"
            | "select-window"
            | "kill-pane"
            | "kill-window"
            | "rename-window"
            | "rename-pane"
            | "list-panes"
            | "list-windows"
            | "display-message"
            | "run"
    )
}

/// Parse `verb` and its arguments. Errors name the command, because the
/// caller is a script and the message is all it gets.
pub fn parse(verb: &str, args: &[String]) -> Result<Cmd> {
    let mut a = Args::new(args);
    let cmd = match verb {
        "send-keys" => {
            let target = a.pane_target()?;
            let literal = a.flag("-l");
            let words = a.rest();
            if words.is_empty() {
                bail!("send-keys needs at least one key");
            }
            let mut keys = vec![];
            for w in words {
                if literal {
                    keys.extend(chars(&w));
                } else {
                    keys.extend(key_word(&w));
                }
            }
            Cmd::SendKeys { target, keys }
        }
        "split-window" => {
            // tmux's -h is "side by side" and -v is "stacked"; -v is the
            // default there, so it is the default here.
            let dir = if a.flag("-h") {
                Dir::Right
            } else {
                a.flag("-v");
                Dir::Down
            };
            a.end(verb)?;
            Cmd::Run(Action::Split(dir))
        }
        "new-window" => {
            a.end(verb)?;
            Cmd::Run(Action::NewTab)
        }
        "select-pane" => {
            let dir = a.direction();
            let target = a.pane_target()?;
            a.end(verb)?;
            match (target, dir) {
                (Some(_), Some(_)) => bail!("select-pane takes a target or a direction, not both"),
                (Some(id), None) => Cmd::SelectPane(id),
                (None, Some(d)) => Cmd::Run(Action::Focus(d)),
                (None, None) => bail!("select-pane needs -t PANE or a direction"),
            }
        }
        "select-window" => {
            if a.flag("-n") {
                Cmd::Run(Action::NextTab)
            } else if a.flag("-p") {
                Cmd::Run(Action::PrevTab)
            } else if a.flag("-l") {
                Cmd::Run(Action::LastTab)
            } else {
                let target = a.window_target()?;
                a.end(verb)?;
                match target {
                    Some(n) => Cmd::SelectWindow(n),
                    None => bail!("select-window needs -t N, -n, -p or -l"),
                }
            }
        }
        "kill-pane" => {
            let target = a.pane_target()?;
            a.end(verb)?;
            Cmd::KillPane(target)
        }
        "kill-window" => {
            let target = a.window_target()?;
            a.end(verb)?;
            Cmd::KillWindow(target)
        }
        "rename-window" => {
            let target = a.window_target()?;
            let name = a.joined();
            if name.is_empty() {
                bail!("rename-window needs a name");
            }
            Cmd::RenameWindow { target, name }
        }
        "rename-pane" => {
            let target = a.pane_target()?;
            let name = a.joined();
            if name.is_empty() {
                bail!("rename-pane needs a name");
            }
            Cmd::RenamePane { target, name }
        }
        "list-panes" => {
            a.end(verb)?;
            Cmd::ListPanes
        }
        "list-windows" => {
            a.end(verb)?;
            Cmd::ListWindows
        }
        "display-message" => {
            let text = a.joined();
            if text.is_empty() {
                bail!("display-message needs some text");
            }
            Cmd::Display(text)
        }
        "run" => {
            let spec = a.joined();
            match Action::from_str(&spec) {
                Ok(action) => Cmd::Run(action),
                Err(e) => bail!("run: {e}"),
            }
        }
        other => bail!("unknown command {other}\n\n{USAGE}"),
    };
    Ok(cmd)
}

/// Arguments with the flags picked out of them as they are asked for. Order
/// does not matter, which is how tmux behaves and what a script expects.
struct Args {
    words: Vec<String>,
}

impl Args {
    fn new(args: &[String]) -> Args {
        Args {
            words: args.to_vec(),
        }
    }

    /// True if `name` was present, removing it.
    fn flag(&mut self, name: &str) -> bool {
        match self.words.iter().position(|w| w == name) {
            Some(i) => {
                self.words.remove(i);
                true
            }
            None => false,
        }
    }

    /// The value of `-t`, removed along with the flag.
    fn value(&mut self, name: &str) -> Result<Option<String>> {
        let Some(i) = self.words.iter().position(|w| w == name) else {
            return Ok(None);
        };
        if i + 1 >= self.words.len() {
            bail!("{name} needs a value");
        }
        self.words.remove(i);
        Ok(Some(self.words.remove(i)))
    }

    /// `-t %3` or `-t 3`: a pane id, as `list-panes` prints it.
    fn pane_target(&mut self) -> Result<Option<PaneId>> {
        let Some(v) = self.value("-t")? else {
            return Ok(None);
        };
        let digits = v.strip_prefix('%').unwrap_or(&v);
        match digits.parse() {
            Ok(id) => Ok(Some(id)),
            Err(_) => bail!("not a pane id: {v}"),
        }
    }

    /// `-t 2`: a window, counted from 1 as the status bar counts them.
    fn window_target(&mut self) -> Result<Option<usize>> {
        let Some(v) = self.value("-t")? else {
            return Ok(None);
        };
        match v.parse() {
            Ok(n) if n >= 1 => Ok(Some(n)),
            _ => bail!("not a window number: {v}"),
        }
    }

    fn direction(&mut self) -> Option<Dir> {
        for (flag, dir) in [
            ("-L", Dir::Left),
            ("-R", Dir::Right),
            ("-U", Dir::Up),
            ("-D", Dir::Down),
        ] {
            if self.flag(flag) {
                return Some(dir);
            }
        }
        None
    }

    fn rest(self) -> Vec<String> {
        self.words
    }

    fn joined(self) -> String {
        self.words.join(" ")
    }

    /// Reject leftovers, so a misspelled flag is an error and not silence.
    fn end(&self, verb: &str) -> Result<()> {
        match self.words.first() {
            Some(w) => bail!("{verb}: unexpected argument {w}"),
            None => Ok(()),
        }
    }
}

/// One word of `send-keys`: a key name such as `Enter` or `C-c`, else the
/// word itself, character by character. tmux resolves it the same way.
fn key_word(word: &str) -> Vec<KeyEvent> {
    // tmux spells modifiers `C-x` and `M-x`; ttmux spells them `ctrl+x`.
    // Both are accepted, so a script can be pasted from either world.
    let spelled = match word.split_once('-') {
        Some((m, rest)) if matches!(m, "C" | "M" | "S") && !rest.is_empty() => {
            format!("{}+{rest}", m.to_ascii_lowercase())
        }
        _ => word.to_string(),
    };
    match Chord::from_str(&spelled) {
        // A single character parses as a chord too, and as a name it would
        // lose its case: "A" must stay a capital A.
        Ok(c) if c.mods.is_empty() && word.chars().count() == 1 => chars(word),
        Ok(c) => vec![KeyEvent::new(c.code, c.mods)],
        Err(_) => chars(word),
    }
}

fn chars(text: &str) -> Vec<KeyEvent> {
    text.chars()
        .map(|c| KeyEvent::new(crossterm::event::KeyCode::Char(c), KeyModifiers::NONE))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn parsed(line: &str) -> Cmd {
        let words = argv(line);
        parse(&words[0], &words[1..]).expect(line)
    }

    #[test]
    fn send_keys_reads_names_modifiers_and_plain_text() {
        let Cmd::SendKeys { target, keys } = parsed("send-keys -t %2 hi Enter C-c") else {
            panic!("not send-keys");
        };
        assert_eq!(target, Some(2));
        assert_eq!(
            keys,
            vec![
                KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            ]
        );
    }

    #[test]
    fn literal_send_keys_types_a_key_name_instead_of_pressing_it() {
        let Cmd::SendKeys { keys, .. } = parsed("send-keys -l Up") else {
            panic!("not send-keys");
        };
        let text: String = keys
            .iter()
            .filter_map(|k| match k.code {
                KeyCode::Char(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Up");
    }

    #[test]
    fn a_single_capital_letter_keeps_its_case() {
        let Cmd::SendKeys { keys, .. } = parsed("send-keys A") else {
            panic!("not send-keys");
        };
        assert_eq!(
            keys,
            vec![KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE)]
        );
    }

    #[test]
    fn split_window_follows_tmux_and_stacks_by_default() {
        assert_eq!(parsed("split-window"), Cmd::Run(Action::Split(Dir::Down)));
        assert_eq!(
            parsed("split-window -h"),
            Cmd::Run(Action::Split(Dir::Right))
        );
        assert_eq!(
            parsed("split-window -v"),
            Cmd::Run(Action::Split(Dir::Down))
        );
    }

    #[test]
    fn select_pane_takes_a_target_or_a_direction() {
        assert_eq!(parsed("select-pane -t 3"), Cmd::SelectPane(3));
        assert_eq!(parsed("select-pane -L"), Cmd::Run(Action::Focus(Dir::Left)));
        let words = argv("select-pane -t 3 -L");
        assert!(parse(&words[0], &words[1..]).is_err());
    }

    #[test]
    fn run_takes_any_action_the_keymap_takes() {
        assert_eq!(parsed("run toggle-zoom"), Cmd::Run(Action::ToggleZoom));
        assert_eq!(
            parsed("run split right"),
            Cmd::Run(Action::Split(Dir::Right))
        );
        let words = argv("run not-an-action");
        assert!(parse(&words[0], &words[1..]).is_err());
    }

    #[test]
    fn a_name_with_spaces_survives_the_shell_splitting_it() {
        assert_eq!(
            parsed("rename-window -t 2 build and test"),
            Cmd::RenameWindow {
                target: Some(2),
                name: "build and test".into(),
            }
        );
    }

    #[test]
    fn a_misspelled_flag_is_an_error_rather_than_silence() {
        assert!(parse("list-panes", &["-x".into()]).is_err());
        assert!(parse("kill-pane", &["-t".into(), "%1".into(), "oops".into()]).is_err());
    }

    #[test]
    fn a_bad_target_is_an_error_rather_than_pane_zero() {
        let words = argv("kill-pane -t nope");
        assert!(parse(&words[0], &words[1..]).is_err());
        let words = argv("select-window -t 0");
        assert!(parse(&words[0], &words[1..]).is_err());
    }
}
