//! Key resolution (bindings, tmux-style prefix) and terminal input encoding.
//!
//! Two jobs: decide whether a key is a ttmux binding or belongs to the pane
//! (`Keys`), and turn the keys/mouse events that belong to the pane into the
//! bytes a real terminal would write to the pty (`encode_key`/`encode_mouse`).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use vt100::{MouseProtocolEncoding, MouseProtocolMode};

use crate::action::Action;
use crate::config::{Binding, Chord, Config};

/// What a key press means to the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Run this action.
    Action(Action),
    /// A prefix chord was swallowed; the next key completes the binding.
    Pending,
    /// Not ours — forward it to the focused pane.
    Passthrough,
}

/// Resolves key events against the configured keymap.
pub struct Keys {
    map: BTreeMap<Binding, Action>,
    prefixes: Vec<Chord>,
    timeout: Duration,
    pending: Option<(Chord, Instant)>,
}

impl Keys {
    pub fn new(cfg: &Config) -> Keys {
        let (map, _errors) = cfg.keymap();
        Keys {
            map,
            prefixes: cfg.prefixes(),
            timeout: Duration::from_millis(cfg.general.prefix_timeout_ms),
            pending: None,
        }
    }

    /// Rebuild from a changed config; any half-typed binding is dropped.
    pub fn reload(&mut self, cfg: &Config) {
        *self = Keys::new(cfg);
    }

    /// Note the unmatched-second-chord case: the prefix followed by something
    /// nothing is bound to comes back as `Passthrough` with `pending()` false.
    /// The caller then
    /// sends the literal key to the pane via [`encode_key`] on the event it
    /// just passed in — which is exactly the tmux "send the prefix" behaviour.
    pub fn resolve(&mut self, ev: KeyEvent) -> Resolution {
        self.resolve_at(ev, Instant::now())
    }

    /// True while a prefix chord is waiting for its second chord.
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The prefix being held, for the which-key popup that lists what can
    /// follow it.
    pub fn pending_prefix(&self) -> Option<&Chord> {
        self.pending.as_ref().map(|(c, _)| c)
    }

    /// `resolve` with an injectable clock, so the timeout is testable.
    fn resolve_at(&mut self, ev: KeyEvent, now: Instant) -> Resolution {
        let chord = Chord::from_event(ev);

        if let Some((prefix, at)) = self.pending.take() {
            if now.duration_since(at) <= self.timeout {
                return match self.map.get(&Binding(vec![prefix, chord])) {
                    Some(action) => Resolution::Action(action.clone()),
                    None => Resolution::Passthrough,
                };
            }
            // Expired: treat this key as if nothing were pending.
        }

        if let Some(action) = self.map.get(&Binding(vec![chord])) {
            Resolution::Action(action.clone())
        } else if self.prefixes.contains(&chord) {
            self.pending = Some((chord, now));
            Resolution::Pending
        } else {
            Resolution::Passthrough
        }
    }
}

// ------------------------------------------------------------------ keyboard

/// xterm modifier parameter: `1 + (shift=1 | alt=2 | ctrl=4)`.
fn mod_param(m: KeyModifiers) -> u8 {
    1 + u8::from(m.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(m.contains(KeyModifiers::ALT))
        + 4 * u8::from(m.contains(KeyModifiers::CONTROL))
}

/// `ESC [ <final>`, `ESC O <final>` or, when modified, `ESC [ 1 ; m <final>`.
fn cursor_key(final_byte: u8, m: KeyModifiers, app_cursor_keys: bool) -> Vec<u8> {
    let p = mod_param(m);
    if p > 1 {
        format!("\x1b[1;{p}{}", final_byte as char).into_bytes()
    } else if app_cursor_keys {
        vec![0x1b, b'O', final_byte]
    } else {
        vec![0x1b, b'[', final_byte]
    }
}

/// `ESC [ n ~` or, when modified, `ESC [ n ; m ~`.
fn tilde_key(n: u8, m: KeyModifiers) -> Vec<u8> {
    let p = mod_param(m);
    if p > 1 {
        format!("\x1b[{n};{p}~").into_bytes()
    } else {
        format!("\x1b[{n}~").into_bytes()
    }
}

/// Control byte for `ctrl+<char>`, if the pair has one.
fn ctrl_byte(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        ' ' | '@' => 0x00,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' | '/' => 0x1f,
        _ => return None,
    })
}

/// Encode a key event the way a terminal would, for writing to the pty.
pub fn encode_key(ev: KeyEvent, app_cursor_keys: bool) -> Vec<u8> {
    if matches!(ev.kind, KeyEventKind::Release) {
        return vec![];
    }
    let m = ev.modifiers;
    let alt = m.contains(KeyModifiers::ALT);
    let ctrl = m.contains(KeyModifiers::CONTROL);

    let mut out = Vec::new();
    match ev.code {
        KeyCode::Char(c) => {
            if alt {
                out.push(0x1b);
            }
            match ctrl.then(|| ctrl_byte(c)).flatten() {
                Some(b) => out.push(b),
                None => out.extend_from_slice(c.to_string().as_bytes()),
            }
        }
        KeyCode::Enter => {
            if alt {
                out.push(0x1b);
            }
            out.push(b'\r');
        }
        KeyCode::Tab => {
            if alt {
                out.push(0x1b);
            }
            out.push(b'\t');
        }
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => {
            if alt {
                out.push(0x1b);
            }
            out.push(if ctrl { 0x08 } else { 0x7f });
        }
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Up => out = cursor_key(b'A', m, app_cursor_keys),
        KeyCode::Down => out = cursor_key(b'B', m, app_cursor_keys),
        KeyCode::Right => out = cursor_key(b'C', m, app_cursor_keys),
        KeyCode::Left => out = cursor_key(b'D', m, app_cursor_keys),
        KeyCode::Home => out = cursor_key(b'H', m, app_cursor_keys),
        KeyCode::End => out = cursor_key(b'F', m, app_cursor_keys),
        KeyCode::Insert => out = tilde_key(2, m),
        KeyCode::Delete => out = tilde_key(3, m),
        KeyCode::PageUp => out = tilde_key(5, m),
        KeyCode::PageDown => out = tilde_key(6, m),
        KeyCode::F(n @ 1..=4) => {
            let final_byte = b'P' + (n - 1);
            let p = mod_param(m);
            if p > 1 {
                out.extend_from_slice(format!("\x1b[1;{p}").as_bytes());
                out.push(final_byte);
            } else {
                out.extend_from_slice(&[0x1b, b'O', final_byte]);
            }
        }
        KeyCode::F(n @ 5..=12) => {
            const CODES: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];
            out = tilde_key(CODES[(n - 5) as usize], m);
        }
        _ => {}
    }
    out
}

// --------------------------------------------------------------------- mouse

/// Encode a mouse event the way the guest asked to receive it.
///
/// `col`/`row` are zero-based within the pane; every wire format is
/// one-based. Returns `None` when the guest's mode does not report this
/// event at all, or when the coordinates do not fit the encoding.
pub fn encode_mouse(
    ev: MouseEvent,
    col: u16,
    row: u16,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    if mode == MouseProtocolMode::None {
        return None;
    }
    let motion = matches!(
        mode,
        MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
    );
    let button = |b: MouseButton| match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (mut code, release) = match ev.kind {
        MouseEventKind::Down(b) => (button(b), false),
        // Press-only guests parse no release report, so one is noise at best
        // and a stray character at worst.
        MouseEventKind::Up(_) if mode == MouseProtocolMode::Press => return None,
        MouseEventKind::Up(b) => (button(b), true),
        MouseEventKind::Drag(_) if !motion => return None,
        MouseEventKind::Drag(b) => (button(b) + 32, false),
        MouseEventKind::Moved if mode != MouseProtocolMode::AnyMotion => return None,
        // No button held: button bits are the 3 that also means release.
        MouseEventKind::Moved => (35, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        // From tuios: a trackpad leaks sideways drift into nearly every
        // vertical scroll, and forwarding that unasked walks the guest
        // sideways, so horizontal wheel counts only when modified.
        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight if ev.modifiers.is_empty() => {
            return None
        }
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };
    if ev.modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if ev.modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    let (col, row) = (col as u32 + 1, row as u32 + 1);
    match encoding {
        MouseProtocolEncoding::Sgr => {
            let end = if release { 'm' } else { 'M' };
            Some(format!("\x1b[<{code};{col};{row}{end}").into_bytes())
        }
        // The older encodings have no release code: button bits 3 is it.
        MouseProtocolEncoding::Utf8 => {
            let mut out = b"\x1b[M".to_vec();
            for n in [
                32 + u32::from(code | if release { 3 } else { 0 }),
                32 + col,
                32 + row,
            ] {
                out.extend_from_slice(char::from_u32(n)?.to_string().as_bytes());
            }
            Some(out)
        }
        MouseProtocolEncoding::Default => {
            // One byte per field, so a click past column 223 is unreportable.
            if col > 223 || row > 223 {
                return None;
            }
            Some(vec![
                0x1b,
                b'[',
                b'M',
                32 + (code | if release { 3 } else { 0 }),
                32 + col as u8,
                32 + row as u8,
            ])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Dir;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn mouse(kind: MouseEventKind, mods: KeyModifiers) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: mods,
        }
    }

    // ------------------------------------------------------------ resolution

    #[test]
    fn prefix_then_key_runs_the_binding() {
        let mut k = Keys::new(&Config::default());
        let t = Instant::now();
        assert_eq!(k.resolve_at(ctrl('t'), t), Resolution::Pending);
        assert!(k.pending());
        assert_eq!(
            k.resolve_at(ch('%'), t),
            Resolution::Action(Action::Split(Dir::Right))
        );
        assert!(!k.pending());
    }

    #[test]
    fn prefix_expires_after_the_timeout() {
        let mut k = Keys::new(&Config::default());
        let t = Instant::now();
        assert_eq!(k.resolve_at(ctrl('t'), t), Resolution::Pending);
        let late = t + Duration::from_millis(2000);
        // `%` on its own is not bound, so the stale prefix is simply dropped.
        assert_eq!(k.resolve_at(ch('%'), late), Resolution::Passthrough);
        assert!(!k.pending());
    }

    #[test]
    fn unbound_key_after_prefix_passes_through() {
        let mut k = Keys::new(&Config::default());
        let t = Instant::now();
        k.resolve_at(ctrl('t'), t);
        assert_eq!(k.resolve_at(ch('9'), t), Resolution::Passthrough);
        assert!(!k.pending());
    }

    #[test]
    fn an_unbound_second_chord_passes_through() {
        let mut k = Keys::new(&Config::default());
        let t = Instant::now();
        k.resolve_at(ctrl('t'), t);
        assert_eq!(k.resolve_at(ctrl('g'), t), Resolution::Passthrough);
        assert!(!k.pending());
        assert_eq!(encode_key(ctrl('a'), false), b"\x01");
    }

    #[test]
    fn the_prefix_twice_is_a_binding_of_its_own() {
        // Their tmux binds `C-t C-t` to last-window and `C-t t` to
        // send-prefix, so the double prefix is not a literal escape here.
        let mut k = Keys::new(&Config::default());
        let t = Instant::now();
        k.resolve_at(ctrl('t'), t);
        assert_eq!(
            k.resolve_at(ctrl('t'), t),
            Resolution::Action(Action::LastTab)
        );
    }

    #[test]
    fn direct_binding_needs_no_prefix() {
        let mut k = Keys::new(&Config::default());
        assert_eq!(
            k.resolve_at(key(KeyCode::Left, KeyModifiers::ALT), Instant::now()),
            Resolution::Action(Action::Focus(Dir::Left))
        );
    }

    #[test]
    fn plain_typing_passes_through() {
        let mut k = Keys::new(&Config::default());
        let t = Instant::now();
        for c in "hello".chars() {
            assert_eq!(k.resolve_at(ch(c), t), Resolution::Passthrough);
        }
    }

    #[test]
    fn reload_clears_pending_state() {
        let mut k = Keys::new(&Config::default());
        k.resolve_at(ctrl('t'), Instant::now());
        k.reload(&Config::default());
        assert!(!k.pending());
    }

    // -------------------------------------------------------------- encoding

    #[test]
    fn plain_and_alt_chars() {
        assert_eq!(encode_key(ch('a'), false), b"a");
        assert_eq!(encode_key(ch('é'), false), "é".as_bytes());
        assert_eq!(
            encode_key(key(KeyCode::Char('x'), KeyModifiers::ALT), false),
            b"\x1bx"
        );
    }

    #[test]
    fn control_characters() {
        assert_eq!(encode_key(ctrl('c'), false), vec![0x03]);
        assert_eq!(encode_key(ctrl('a'), false), vec![0x01]);
        assert_eq!(encode_key(ctrl('z'), false), vec![0x1a]);
        assert_eq!(encode_key(ctrl(' '), false), vec![0x00]);
        assert_eq!(encode_key(ctrl('@'), false), vec![0x00]);
        assert_eq!(encode_key(ctrl('['), false), vec![0x1b]);
        assert_eq!(encode_key(ctrl('\\'), false), vec![0x1c]);
        assert_eq!(encode_key(ctrl(']'), false), vec![0x1d]);
        assert_eq!(encode_key(ctrl('^'), false), vec![0x1e]);
        assert_eq!(encode_key(ctrl('_'), false), vec![0x1f]);
        assert_eq!(encode_key(ctrl('/'), false), vec![0x1f]);
    }

    #[test]
    fn simple_special_keys() {
        let n = KeyModifiers::NONE;
        assert_eq!(encode_key(key(KeyCode::Enter, n), false), b"\r");
        assert_eq!(encode_key(key(KeyCode::Tab, n), false), b"\t");
        assert_eq!(encode_key(key(KeyCode::BackTab, n), false), b"\x1b[Z");
        assert_eq!(encode_key(key(KeyCode::Esc, n), false), b"\x1b");
    }

    #[test]
    fn backspace_is_del() {
        assert_eq!(
            encode_key(key(KeyCode::Backspace, KeyModifiers::NONE), false),
            vec![0x7f]
        );
    }

    #[test]
    fn arrows_in_normal_mode() {
        let n = KeyModifiers::NONE;
        assert_eq!(encode_key(key(KeyCode::Up, n), false), b"\x1b[A");
        assert_eq!(encode_key(key(KeyCode::Down, n), false), b"\x1b[B");
        assert_eq!(encode_key(key(KeyCode::Right, n), false), b"\x1b[C");
        assert_eq!(encode_key(key(KeyCode::Left, n), false), b"\x1b[D");
    }

    #[test]
    fn arrows_in_application_cursor_mode() {
        let n = KeyModifiers::NONE;
        assert_eq!(encode_key(key(KeyCode::Up, n), true), b"\x1bOA");
        assert_eq!(encode_key(key(KeyCode::Left, n), true), b"\x1bOD");
        assert_eq!(encode_key(key(KeyCode::Home, n), true), b"\x1bOH");
        assert_eq!(encode_key(key(KeyCode::End, n), true), b"\x1bOF");
    }

    #[test]
    fn home_end_and_tilde_keys() {
        let n = KeyModifiers::NONE;
        assert_eq!(encode_key(key(KeyCode::Home, n), false), b"\x1b[H");
        assert_eq!(encode_key(key(KeyCode::End, n), false), b"\x1b[F");
        assert_eq!(encode_key(key(KeyCode::Insert, n), false), b"\x1b[2~");
        assert_eq!(encode_key(key(KeyCode::Delete, n), false), b"\x1b[3~");
        assert_eq!(encode_key(key(KeyCode::PageUp, n), false), b"\x1b[5~");
        assert_eq!(encode_key(key(KeyCode::PageDown, n), false), b"\x1b[6~");
    }

    #[test]
    fn function_keys() {
        let n = KeyModifiers::NONE;
        assert_eq!(encode_key(key(KeyCode::F(1), n), false), b"\x1bOP");
        assert_eq!(encode_key(key(KeyCode::F(4), n), false), b"\x1bOS");
        assert_eq!(encode_key(key(KeyCode::F(5), n), false), b"\x1b[15~");
        assert_eq!(encode_key(key(KeyCode::F(6), n), false), b"\x1b[17~");
        assert_eq!(encode_key(key(KeyCode::F(11), n), false), b"\x1b[23~");
        assert_eq!(encode_key(key(KeyCode::F(12), n), false), b"\x1b[24~");
    }

    #[test]
    fn ctrl_right_uses_the_modifier_parameter() {
        assert_eq!(
            encode_key(key(KeyCode::Right, KeyModifiers::CONTROL), false),
            b"\x1b[1;5C"
        );
    }

    #[test]
    fn alt_up_uses_the_modifier_parameter() {
        assert_eq!(
            encode_key(key(KeyCode::Up, KeyModifiers::ALT), false),
            b"\x1b[1;3A"
        );
    }

    #[test]
    fn modified_arrows_ignore_application_cursor_mode() {
        assert_eq!(
            encode_key(key(KeyCode::Left, KeyModifiers::SHIFT), true),
            b"\x1b[1;2D"
        );
    }

    #[test]
    fn shift_f5_and_modified_tilde_keys() {
        assert_eq!(
            encode_key(key(KeyCode::F(5), KeyModifiers::SHIFT), false),
            b"\x1b[15;2~"
        );
        assert_eq!(
            encode_key(key(KeyCode::Delete, KeyModifiers::CONTROL), false),
            b"\x1b[3;5~"
        );
        assert_eq!(
            encode_key(
                key(KeyCode::F(1), KeyModifiers::CONTROL | KeyModifiers::SHIFT),
                false
            ),
            b"\x1b[1;6P"
        );
    }

    #[test]
    fn key_release_encodes_to_nothing() {
        let mut ev = ch('a');
        ev.kind = KeyEventKind::Release;
        assert!(encode_key(ev, false).is_empty());
        let mut ev = ch('a');
        ev.kind = KeyEventKind::Repeat;
        assert_eq!(encode_key(ev, false), b"a");
    }

    // ----------------------------------------------------------------- mouse

    /// The common case: a guest on SGR with button motion.
    fn sgr(ev: MouseEvent, col: u16, row: u16) -> Option<Vec<u8>> {
        encode_mouse(
            ev,
            col,
            row,
            MouseProtocolMode::ButtonMotion,
            MouseProtocolEncoding::Sgr,
        )
    }

    #[test]
    fn mouse_left_press_and_release() {
        let n = KeyModifiers::NONE;
        assert_eq!(
            sgr(mouse(MouseEventKind::Down(MouseButton::Left), n), 4, 9).unwrap(),
            b"\x1b[<0;5;10M"
        );
        assert_eq!(
            sgr(mouse(MouseEventKind::Up(MouseButton::Left), n), 0, 0).unwrap(),
            b"\x1b[<0;1;1m"
        );
    }

    #[test]
    fn mouse_wheel_and_modifiers() {
        let n = KeyModifiers::NONE;
        assert_eq!(
            sgr(mouse(MouseEventKind::ScrollUp, n), 2, 3).unwrap(),
            b"\x1b[<64;3;4M"
        );
        assert_eq!(
            sgr(
                mouse(MouseEventKind::ScrollDown, KeyModifiers::CONTROL),
                2,
                3
            )
            .unwrap(),
            b"\x1b[<81;3;4M"
        );
    }

    #[test]
    fn mouse_drag_and_plain_motion() {
        let n = KeyModifiers::NONE;
        assert_eq!(
            sgr(mouse(MouseEventKind::Drag(MouseButton::Left), n), 7, 1).unwrap(),
            b"\x1b[<32;8;2M"
        );
        assert_eq!(sgr(mouse(MouseEventKind::Moved, n), 7, 1), None);
    }

    #[test]
    fn mouse_encoding_follows_what_the_guest_declared() {
        let ev = mouse(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE);
        assert_eq!(
            encode_mouse(
                ev,
                4,
                9,
                MouseProtocolMode::Press,
                MouseProtocolEncoding::Default
            )
            .unwrap(),
            vec![0x1b, b'[', b'M', 32, 32 + 5, 32 + 10]
        );
        assert_eq!(
            encode_mouse(
                ev,
                4,
                9,
                MouseProtocolMode::Press,
                MouseProtocolEncoding::Utf8
            )
            .unwrap(),
            "\x1b[M\u{20}\u{25}\u{2a}".as_bytes()
        );
        // Past 223 columns the one-byte encoding has nothing to say.
        assert_eq!(
            encode_mouse(
                ev,
                300,
                0,
                MouseProtocolMode::Press,
                MouseProtocolEncoding::Default
            ),
            None
        );
        // Utf8 reaches further, and a release is button 3 there.
        let up = mouse(MouseEventKind::Up(MouseButton::Left), KeyModifiers::NONE);
        assert_eq!(
            encode_mouse(
                up,
                0,
                0,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Utf8
            )
            .unwrap(),
            "\x1b[M\u{23}\u{21}\u{21}".as_bytes()
        );
    }

    #[test]
    fn motion_and_release_are_dropped_in_press_only_mode() {
        let n = KeyModifiers::NONE;
        let each = [
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Moved,
        ];
        for kind in each {
            assert_eq!(
                encode_mouse(
                    mouse(kind, n),
                    0,
                    0,
                    MouseProtocolMode::Press,
                    MouseProtocolEncoding::Sgr
                ),
                None,
                "{kind:?} should not be reported to a press-only guest"
            );
        }
        // Drag needs a motion mode; bare motion needs any-motion.
        assert!(sgr(mouse(MouseEventKind::Drag(MouseButton::Left), n), 0, 0).is_some());
        assert!(encode_mouse(
            mouse(MouseEventKind::Moved, n),
            0,
            0,
            MouseProtocolMode::AnyMotion,
            MouseProtocolEncoding::Sgr
        )
        .is_some());
        // A guest that asked for nothing gets nothing.
        assert_eq!(
            encode_mouse(
                mouse(MouseEventKind::Down(MouseButton::Left), n),
                0,
                0,
                MouseProtocolMode::None,
                MouseProtocolEncoding::Sgr
            ),
            None
        );
    }

    #[test]
    fn horizontal_wheel_needs_a_modifier() {
        let n = KeyModifiers::NONE;
        assert_eq!(sgr(mouse(MouseEventKind::ScrollLeft, n), 0, 0), None);
        assert_eq!(sgr(mouse(MouseEventKind::ScrollRight, n), 0, 0), None);
        assert_eq!(
            sgr(mouse(MouseEventKind::ScrollLeft, KeyModifiers::SHIFT), 0, 0).unwrap(),
            b"\x1b[<70;1;1M"
        );
    }
}
