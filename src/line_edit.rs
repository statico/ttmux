//! One line of editable text, with the readline keys a shell teaches:
//! ctrl+a/e/b/f/h/d/k/u/w/y/t, alt+b/f/d/u/l/c and alt+backspace.
//!
//! Every text field in ttmux -- the rename prompt, the command palette, a
//! settings value -- routes its keys through [`LineEdit::key`] so they all
//! behave the same way.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What a field draws at its cursor.
pub const CARET: &str = "▏";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LineEdit {
    text: String,
    /// Byte index into `text`, always on a char boundary.
    cursor: usize,
    /// The last thing ctrl+k, ctrl+u, ctrl+w, alt+d or alt+backspace cut,
    /// for ctrl+y to put back.
    // ponytail: one slot, no kill ring or appending runs of kills.
    killed: String,
}

impl LineEdit {
    /// A field holding `text`, with the cursor after it.
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.len();
        Self {
            text,
            cursor,
            killed: String::new(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The text, with `caret` inserted where the cursor is, for drawing.
    pub fn with_caret(&self, caret: &str) -> String {
        let (head, tail) = self.text.split_at(self.cursor);
        format!("{head}{caret}{tail}")
    }

    /// Handle one key. False means this field does not want it, so the caller
    /// can treat it as Enter, Esc, a list movement, or whatever else.
    pub fn key(&mut self, ev: KeyEvent) -> bool {
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let alt = ev.modifiers.contains(KeyModifiers::ALT);
        match ev.code {
            KeyCode::Char(c) if ctrl => match c {
                'a' => self.cursor = 0,
                'e' => self.cursor = self.text.len(),
                'b' => self.cursor = self.prev(),
                'f' => self.cursor = self.next(),
                'h' => self.delete_back(),
                'd' => self.delete_forward(),
                'k' => self.kill(self.cursor, self.text.len()),
                'u' => self.kill(0, self.cursor),
                'w' => self.kill(self.word_left(), self.cursor),
                'y' => {
                    self.text.insert_str(self.cursor, &self.killed);
                    self.cursor += self.killed.len();
                }
                't' => self.transpose(),
                _ => return false,
            },
            // A terminal sends alt as either a modifier or an ESC prefix;
            // crossterm reports both as ALT, so only this arm is needed.
            KeyCode::Char(c) if alt => match c {
                'b' => self.cursor = self.word_left(),
                'f' => self.cursor = self.word_right(),
                'd' => self.kill(self.cursor, self.word_right()),
                'u' => self.recase(|w| w.to_uppercase()),
                'l' => self.recase(|w| w.to_lowercase()),
                'c' => self.recase(|w| {
                    let mut cs = w.chars();
                    cs.next().map_or(String::new(), |f| {
                        f.to_uppercase()
                            .chain(cs.flat_map(char::to_lowercase))
                            .collect()
                    })
                }),
                _ => return false,
            },
            KeyCode::Char(c) => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }
            KeyCode::Backspace if alt || ctrl => self.kill(self.word_left(), self.cursor),
            KeyCode::Backspace => self.delete_back(),
            KeyCode::Delete => self.delete_forward(),
            KeyCode::Left if alt => self.cursor = self.word_left(),
            KeyCode::Right if alt => self.cursor = self.word_right(),
            KeyCode::Left => self.cursor = self.prev(),
            KeyCode::Right => self.cursor = self.next(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            _ => return false,
        }
        true
    }

    /// Cut `from..to` into the kill buffer and leave the cursor at `from`.
    fn kill(&mut self, from: usize, to: usize) {
        self.killed = self.text.drain(from..to).collect();
        self.cursor = from;
    }

    /// Swap the two characters around the cursor and step past them; at the
    /// end of the line, the last two, as readline does.
    fn transpose(&mut self) {
        if self.cursor == self.text.len() {
            self.cursor = self.prev();
        }
        let (a, b) = (self.prev(), self.next());
        if a == self.cursor || b == self.cursor {
            return;
        }
        let pair: String = self.text[a..b].chars().rev().collect();
        self.text.replace_range(a..b, &pair);
        self.cursor = b;
    }

    /// Rewrite the word after the cursor with `f` and move past it.
    fn recase(&mut self, f: impl Fn(&str) -> String) {
        let to = self.word_right();
        let start = self.cursor
            + self.text[self.cursor..to]
                .find(char::is_alphanumeric)
                .unwrap_or(to - self.cursor);
        let word = f(&self.text[start..to]);
        self.text.replace_range(start..to, &word);
        self.cursor = start + word.len();
    }

    fn delete_back(&mut self) {
        let to = self.prev();
        self.text.drain(to..self.cursor);
        self.cursor = to;
    }

    fn delete_forward(&mut self) {
        let to = self.next();
        self.text.drain(self.cursor..to);
    }

    fn prev(&self) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next(&self) -> usize {
        self.text[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |c| self.cursor + c.len_utf8())
    }

    /// The start of the word at or before the cursor: skip whatever
    /// separators sit under it, then the word itself.
    fn word_left(&self) -> usize {
        let mut i = self.cursor;
        while let Some(c) = self.text[..i].chars().next_back() {
            if c.is_alphanumeric() {
                break;
            }
            i -= c.len_utf8();
        }
        while let Some(c) = self.text[..i].chars().next_back() {
            if !c.is_alphanumeric() {
                break;
            }
            i -= c.len_utf8();
        }
        i
    }

    /// The end of the word at or after the cursor, by the same rule.
    fn word_right(&self) -> usize {
        let mut i = self.cursor;
        while let Some(c) = self.text[i..].chars().next() {
            if c.is_alphanumeric() {
                break;
            }
            i += c.len_utf8();
        }
        while let Some(c) = self.text[i..].chars().next() {
            if !c.is_alphanumeric() {
                break;
            }
            i += c.len_utf8();
        }
        i
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn ctrl(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn alt(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::ALT)
    }

    fn typed(s: &str) -> LineEdit {
        let mut e = LineEdit::default();
        for c in s.chars() {
            e.key(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        e
    }

    #[test]
    fn typing_inserts_at_the_cursor_not_at_the_end() {
        let mut e = typed("helo");
        e.key(ctrl('a'));
        e.key(key(KeyCode::Right, KeyModifiers::NONE));
        e.key(key(KeyCode::Char('E'), KeyModifiers::NONE));
        assert_eq!(e.text(), "hEelo");
    }

    #[test]
    fn ctrl_a_and_ctrl_e_jump_to_the_ends() {
        let mut e = typed("abc");
        e.key(ctrl('a'));
        assert_eq!(e.with_caret("|"), "|abc");
        e.key(ctrl('e'));
        assert_eq!(e.with_caret("|"), "abc|");
    }

    #[test]
    fn ctrl_k_and_ctrl_u_kill_each_side_of_the_cursor() {
        let mut e = typed("one two");
        e.key(ctrl('a'));
        e.key(ctrl('f'));
        e.key(ctrl('f'));
        e.key(ctrl('f'));
        e.key(ctrl('k'));
        assert_eq!(e.text(), "one");
        e.key(ctrl('u'));
        assert_eq!(e.text(), "");
    }

    #[test]
    fn ctrl_w_kills_the_word_before_the_cursor() {
        let mut e = typed("some words here");
        e.key(ctrl('w'));
        assert_eq!(e.text(), "some words ");
        e.key(ctrl('w'));
        assert_eq!(e.text(), "some ");
    }

    #[test]
    fn ctrl_d_deletes_forward_and_backspace_deletes_back() {
        let mut e = typed("abc");
        e.key(ctrl('b'));
        e.key(ctrl('d'));
        assert_eq!(e.text(), "ab");
        e.key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(e.text(), "a");
    }

    #[test]
    fn alt_b_and_alt_f_move_by_words() {
        let mut e = typed("alpha beta gamma");
        e.key(alt('b'));
        assert_eq!(e.with_caret("|"), "alpha beta |gamma");
        e.key(alt('b'));
        assert_eq!(e.with_caret("|"), "alpha |beta gamma");
        e.key(alt('f'));
        assert_eq!(e.with_caret("|"), "alpha beta| gamma");
    }

    #[test]
    fn alt_d_kills_the_word_after_the_cursor() {
        let mut e = typed("alpha beta");
        e.key(ctrl('a'));
        e.key(alt('d'));
        assert_eq!(e.text(), " beta");
    }

    #[test]
    fn ctrl_y_yanks_back_what_was_last_killed() {
        let mut e = typed("alpha beta");
        e.key(ctrl('w'));
        e.key(ctrl('a'));
        e.key(ctrl('y'));
        assert_eq!(e.with_caret("|"), "beta|alpha ");
        e.key(ctrl('e'));
        e.key(key(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(e.text(), "", "betaalpha is one word");
        e.key(ctrl('y'));
        assert_eq!(e.text(), "betaalpha ");
    }

    #[test]
    fn ctrl_t_transposes_like_readline() {
        let mut e = typed("abcd");
        e.key(ctrl('t'));
        assert_eq!(e.with_caret("|"), "abdc|");
        e.key(ctrl('a'));
        e.key(ctrl('t'));
        assert_eq!(e.text(), "abdc", "nothing before the cursor to swap");
        e.key(ctrl('f'));
        e.key(ctrl('t'));
        assert_eq!(e.with_caret("|"), "ba|dc");
    }

    #[test]
    fn alt_u_l_c_recase_the_next_word() {
        let mut e = typed("hello wORLD é");
        e.key(ctrl('a'));
        e.key(alt('u'));
        assert_eq!(e.with_caret("|"), "HELLO| wORLD é");
        e.key(alt('c'));
        assert_eq!(e.with_caret("|"), "HELLO World| é");
        e.key(ctrl('a'));
        e.key(alt('l'));
        assert_eq!(e.text(), "hello World é");
    }

    #[test]
    fn multibyte_text_never_splits_a_character() {
        let mut e = typed("héllo →");
        e.key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(e.text(), "héllo ");
        e.key(ctrl('a'));
        e.key(ctrl('f'));
        e.key(ctrl('f'));
        e.key(ctrl('d'));
        assert_eq!(e.text(), "hélo ");
    }

    #[test]
    fn keys_the_field_has_no_use_for_go_back_to_the_caller() {
        let mut e = typed("x");
        assert!(!e.key(key(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(!e.key(key(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(!e.key(key(KeyCode::Up, KeyModifiers::NONE)));
        assert!(!e.key(ctrl('z')));
        assert_eq!(e.text(), "x");
    }
}
