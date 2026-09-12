//! Split a pty byte stream into plain terminal bytes and inline-image escape
//! sequences.
//!
//! vt100 has no notion of kitty/iTerm2/sixel graphics, so it renders their
//! payloads as garbage. [`Scanner`] lifts them out before the emulator sees
//! them; the app replays the captured bytes at the real cursor, the way
//! tmux's `allow-passthrough` does.
//!
//! Recognised framing (the payload is never interpreted):
//! - kitty:  `ESC _ G ... ESC \`
//! - iTerm2: `ESC ] 1337 ; ... BEL` or `... ESC \`
//! - sixel:  `ESC P <params> q ... ESC \`

use std::borrow::Cow;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// Biggest single sequence kept. 4MB of base64 is already a multi-megapixel
/// image; past that the pane is spewing, not drawing.
const MAX_SEQ: usize = 4 * 1024 * 1024;

/// One captured graphics sequence, with the pane cursor cell it started at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub row: u16,
    pub col: u16,
    pub bytes: Vec<u8>,
}

/// Output of [`Scanner::feed`], in stream order.
#[derive(Debug, PartialEq, Eq)]
pub enum Piece<'a> {
    /// Feed to the emulator unchanged.
    Plain(Cow<'a, [u8]>),
    /// A whole graphics sequence, terminator included.
    Image(Vec<u8>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Apc,
    Osc,
    Dcs,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ground,
    /// Inside a prefix that may turn out to be one of ours.
    Maybe,
    Body(Kind),
}

/// Resumable splitter: a sequence may be cut across any number of `feed`s.
pub struct Scanner {
    state: State,
    /// The sequence so far, from its ESC.
    buf: Vec<u8>,
    /// Previous body byte was ESC, so `\` would end the sequence.
    esc: bool,
    /// This sequence blew the size cap; swallow it to its terminator.
    dropped: bool,
}

impl Default for Scanner {
    fn default() -> Self {
        Scanner {
            state: State::Ground,
            buf: Vec::new(),
            esc: false,
            dropped: false,
        }
    }
}

enum Verdict {
    /// Not enough bytes yet to tell.
    Need,
    /// The prefix is complete; everything after it is payload.
    Start(Kind),
    /// Not a graphics sequence.
    No,
}

/// Decide what `buf` (starting with ESC, at least 2 bytes) is becoming.
fn classify(buf: &[u8]) -> Verdict {
    match buf[1] {
        b'_' => match buf.get(2) {
            None => Verdict::Need,
            Some(b'G') => Verdict::Start(Kind::Apc),
            Some(_) => Verdict::No,
        },
        b']' => {
            const PREFIX: &[u8] = b"1337;";
            let tail = &buf[2..];
            if tail.len() > PREFIX.len() || PREFIX[..tail.len()] != *tail {
                Verdict::No
            } else if tail.len() == PREFIX.len() {
                Verdict::Start(Kind::Osc)
            } else {
                Verdict::Need
            }
        }
        // Sixel is the only DCS with a bare numeric parameter list; DECRQSS
        // and friends carry an intermediate byte, so they fall out here.
        b'P' => match buf[buf.len() - 1] {
            b'q' => Verdict::Start(Kind::Dcs),
            b'0'..=b'9' | b';' => Verdict::Need,
            _ if buf.len() == 2 => Verdict::Need,
            _ => Verdict::No,
        },
        _ => Verdict::No,
    }
}

impl Scanner {
    pub fn new() -> Scanner {
        Scanner::default()
    }

    /// Split one read of pty output. Plain pieces borrow `input` where they
    /// can; only a candidate that spanned reads is copied.
    pub fn feed<'a>(&mut self, input: &'a [u8]) -> Vec<Piece<'a>> {
        let mut out = Vec::new();
        // Start of the run of plain bytes we are inside of, when in Ground.
        let mut plain = 0usize;
        let mut i = 0usize;
        while i < input.len() {
            let b = input[i];
            i += 1;
            match self.state {
                State::Ground => {
                    if b == ESC {
                        if plain < i - 1 {
                            out.push(Piece::Plain(Cow::Borrowed(&input[plain..i - 1])));
                        }
                        self.state = State::Maybe;
                        self.buf.clear();
                        self.buf.push(ESC);
                    }
                }
                State::Maybe => {
                    self.buf.push(b);
                    let verdict = if self.buf.len() >= MAX_SEQ {
                        Verdict::No
                    } else {
                        classify(&self.buf)
                    };
                    match verdict {
                        Verdict::Need => {}
                        Verdict::Start(k) => self.state = State::Body(k),
                        Verdict::No => {
                            // A fresh ESC here opens the next candidate rather
                            // than belonging to the rejected one.
                            let keep = b == ESC;
                            let n = self.buf.len() - keep as usize;
                            out.push(Piece::Plain(Cow::Owned(self.buf[..n].to_vec())));
                            self.buf.clear();
                            if keep {
                                self.buf.push(ESC);
                            } else {
                                self.state = State::Ground;
                                plain = i;
                            }
                        }
                    }
                }
                State::Body(k) => {
                    // ponytail: over the cap we swallow to the terminator
                    // instead of flushing garbage at vt100; a sequence that
                    // never terminates therefore eats the pane's output.
                    if self.dropped {
                    } else if self.buf.len() >= MAX_SEQ {
                        self.dropped = true;
                        self.buf = Vec::new();
                    } else {
                        self.buf.push(b);
                    }
                    if b == ESC {
                        self.esc = true;
                        continue;
                    }
                    let done = (self.esc && b == b'\\') || (k == Kind::Osc && b == BEL);
                    self.esc = false;
                    if done {
                        if !std::mem::take(&mut self.dropped) {
                            out.push(Piece::Image(std::mem::take(&mut self.buf)));
                        }
                        self.buf.clear();
                        self.state = State::Ground;
                        plain = i;
                    }
                }
            }
        }
        if self.state == State::Ground && plain < input.len() {
            out.push(Piece::Plain(Cow::Borrowed(&input[plain..])));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `chunks` and return (everything vt100 would see, captured images).
    fn run(chunks: &[&[u8]]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut s = Scanner::new();
        let (mut plain, mut imgs) = (Vec::new(), Vec::new());
        for c in chunks {
            for p in s.feed(c) {
                match p {
                    Piece::Plain(b) => plain.extend_from_slice(&b),
                    Piece::Image(b) => imgs.push(b),
                }
            }
        }
        (plain, imgs)
    }

    const KITTY: &[u8] = b"\x1b_Ga=T,f=100;iVBORw0\x1b\\";
    const ITERM: &[u8] = b"\x1b]1337;File=inline=1:AAAA\x07";
    const SIXEL: &[u8] = b"\x1bP0;1;0q#0;2;0;0;0#0~~@@vv@@~~$\x1b\\";

    #[test]
    fn each_protocol_is_captured_whole() {
        for seq in [KITTY, ITERM, SIXEL] {
            let mut input = b"before".to_vec();
            input.extend_from_slice(seq);
            input.extend_from_slice(b"after");
            let (plain, imgs) = run(&[&input]);
            assert_eq!(plain, b"beforeafter", "{:?}", seq);
            assert_eq!(imgs, vec![seq.to_vec()], "{:?}", seq);
        }
    }

    #[test]
    fn iterm_also_ends_at_st() {
        let (plain, imgs) = run(&[b"\x1b]1337;File=inline=1:AA\x1b\\x"]);
        assert_eq!(plain, b"x");
        assert_eq!(imgs, vec![b"\x1b]1337;File=inline=1:AA\x1b\\".to_vec()]);
    }

    #[test]
    fn a_sequence_survives_being_split() {
        for seq in [KITTY, ITERM, SIXEL] {
            // Three chunks, cut inside the prefix and inside the payload.
            let (a, b) = seq.split_at(2);
            let (b, c) = b.split_at(5);
            assert_eq!(run(&[a, b, c]).1, vec![seq.to_vec()]);

            // One byte at a time, which cuts every boundary there is.
            let bytes: Vec<&[u8]> = seq.chunks(1).collect();
            let (plain, imgs) = run(&bytes);
            assert!(plain.is_empty(), "{plain:?}");
            assert_eq!(imgs, vec![seq.to_vec()]);
        }
    }

    #[test]
    fn ordinary_escapes_pass_through() {
        // CSI, a non-1337 OSC, a non-sixel DCS, an APC that is not kitty, and
        // a lone trailing ESC (still undecided, so it is held, not mangled).
        let input: &[u8] =
            b"\x1b[1;31mred\x1b[0m\x1b]0;title\x07\x1bP$qm\x1b\\\x1b_Zfoo\x1b\\hi\x1b";
        let (plain, imgs) = run(&[input]);
        assert!(imgs.is_empty());
        assert_eq!(plain, &input[..input.len() - 1]);
        // Byte-at-a-time must not change what vt100 sees.
        let bytes: Vec<&[u8]> = input.chunks(1).collect();
        assert_eq!(run(&bytes).0, plain);
    }

    #[test]
    fn esc_esc_keeps_the_second_esc() {
        let (plain, imgs) = run(&[b"\x1b\x1b_Gx\x1b\\z"]);
        assert_eq!(plain, b"\x1bz");
        assert_eq!(imgs, vec![b"\x1b_Gx\x1b\\".to_vec()]);
    }

    #[test]
    fn an_oversized_sequence_is_dropped_and_the_scanner_recovers() {
        let mut input = b"\x1b_Ga=T;".to_vec();
        input.resize(MAX_SEQ + 4096, b'A');
        input.extend_from_slice(b"\x1b\\tail");
        let (plain, imgs) = run(&[&input]);
        assert!(imgs.is_empty(), "oversized sequence was kept");
        assert_eq!(plain, b"tail");
        // And the next, well-sized one still lands.
        let (_, imgs) = run(&[&input, KITTY]);
        assert_eq!(imgs, vec![KITTY.to_vec()]);
    }
}
