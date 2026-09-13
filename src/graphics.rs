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

/// Biggest kitty image kept once its chunks are joined: a 4K RGBA frame is
/// about 45MB of base64.
pub const MAX_IMAGE: usize = 64 * 1024 * 1024;

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

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum State {
    #[default]
    Ground,
    /// Inside a prefix that may turn out to be one of ours.
    Maybe,
    Body(Kind),
    /// A kitty chunk was cut by other output: what follows is that output
    /// and then the chunk's base64 tail up to its ST.
    Resync,
}

/// Longest `Resync` waits for the ST: a kitty chunk carries at most 4096
/// bytes of payload, and a status line is well under the rest.
const RESYNC_MAX: usize = 8192;

/// Resumable splitter: a sequence may be cut across any number of `feed`s.
#[derive(Default)]
pub struct Scanner {
    state: State,
    /// The sequence so far, from its ESC.
    buf: Vec<u8>,
    /// Previous body byte was ESC, so `\` would end the sequence.
    esc: bool,
    /// This sequence blew the size cap; swallow it to its terminator.
    dropped: bool,
    /// A kitty image sent in chunks (`m=1`) so far. The chunks only mean
    /// something together: replayed one by one, the host sees headless
    /// chunks and cut-off transmissions, and prints base64.
    chunks: Vec<u8>,
    /// The image being chunked blew a cap; swallow the rest of its chunks.
    chunks_dropped: bool,
    /// The dropped sequence said more chunks follow, read before its bytes
    /// were released.
    dropped_more: bool,
}

/// Whether a kitty APC is part of a chunked transmission at all.
fn is_chunk(seq: &[u8]) -> bool {
    let body = &seq[3..];
    let end = body.iter().position(|b| *b == b';' || *b == ESC);
    body[..end.unwrap_or(body.len())]
        .split(|b| *b == b',')
        .any(|kv| kv.starts_with(b"m="))
}

/// Whether a kitty APC says more chunks follow.
fn more_chunks(seq: &[u8]) -> bool {
    let body = &seq[3..];
    let end = body.iter().position(|b| *b == b';' || *b == ESC);
    body[..end.unwrap_or(body.len())]
        .split(|b| *b == b',')
        .any(|kv| kv == b"m=1")
}

/// Whether a captured sequence only draws. Kitty can also make the host
/// read a file or shared memory (`t=f`, `t=t`, `t=s`), and OSC 1337 carries
/// far more than images (downloads, user vars, focus stealing), so only
/// direct kitty data, inline iTerm2 files and sixel pass. tmux blocks all of
/// it by default.
// ponytail: iTerm2 multipart transfers (`MultipartFile`) are dropped too;
// allow them with `inline=1` if a real tool needs them.
pub fn is_safe(seq: &[u8]) -> bool {
    if seq.starts_with(b"\x1b_G") {
        // A joined chunked image is several APCs; each one's keys count.
        return seq.split(|b| *b == ESC).all(|apc| {
            let Some(keys) = apc.strip_prefix(b"_G") else {
                return true;
            };
            let end = keys.iter().position(|b| *b == b';').unwrap_or(keys.len());
            keys[..end]
                .split(|b| *b == b',')
                .all(|kv| !kv.starts_with(b"t=") || kv == b"t=d")
        });
    }
    if let Some(rest) = seq.strip_prefix(b"\x1b]1337;File=") {
        let end = rest.iter().position(|b| *b == b':').unwrap_or(rest.len());
        return rest[..end]
            .split(|b| *b == b';')
            .any(|kv| kv == b"inline=1");
    }
    seq.starts_with(b"\x1bP")
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
                State::Resync => {
                    self.buf.push(b);
                    let n = self.buf.len();
                    if self.buf.ends_with(b"\x1b\\") {
                        // The tail is the base64 run before the ST; the
                        // other output is everything before that.
                        // ponytail: output that itself ends in base64
                        // letters loses them; mpv's ends in "%)".
                        let text = &self.buf[..n - 2];
                        let b64 = |c: &u8| c.is_ascii_alphanumeric() || b"+/=".contains(c);
                        let keep = text.iter().rposition(|c| !b64(c)).map_or(0, |at| at + 1);
                        out.push(Piece::Plain(Cow::Owned(text[..keep].to_vec())));
                        self.buf.clear();
                        self.state = State::Ground;
                        plain = i;
                    } else if n >= RESYNC_MAX {
                        // No ST: it was not a cut chunk after all.
                        out.push(Piece::Plain(Cow::Owned(std::mem::take(&mut self.buf))));
                        self.state = State::Ground;
                        plain = i;
                    }
                }
                State::Body(k) => {
                    // An ESC that does not start ST cancels the string, as in
                    // any terminal: mpv's status line, written from another
                    // thread, lands inside its image chunks. Kept, the image
                    // carries the status line to the host on every replay and
                    // the host prints the rest of the base64 over the window.
                    if self.esc && b != b'\\' {
                        self.esc = false;
                        let chunked = k == Kind::Apc
                            && (!self.chunks.is_empty() || self.dropped || is_chunk(&self.buf));
                        if k == Kind::Apc {
                            self.chunks_dropped = !self.chunks.is_empty()
                                || if self.dropped {
                                    self.dropped_more
                                } else {
                                    more_chunks(&self.buf)
                                };
                            self.chunks = Vec::new();
                        }
                        // mpv's case: the interrupted chunk's tail is still to
                        // come, and a terminal would print it. Wait for it.
                        if chunked {
                            self.dropped = false;
                            self.buf = vec![ESC, b];
                            self.state = State::Resync;
                            continue;
                        }
                        // vt100 gets the cancelled bytes and ignores them the
                        // same way; this ESC opens whatever comes next.
                        if !std::mem::take(&mut self.dropped) {
                            let mut body = std::mem::take(&mut self.buf);
                            body.pop();
                            out.push(Piece::Plain(Cow::Owned(body)));
                        }
                        self.buf = vec![ESC];
                        self.state = State::Maybe;
                        i -= 1;
                        continue;
                    }
                    // ponytail: an unterminated sequence eats the pane's
                    // output — under the cap it is buffered, over the cap it
                    // is swallowed, and either way nothing reaches vt100 until
                    // the ST arrives. Flushing the partial bytes instead would
                    // paint base64 over the screen, which is worse. Upgrade
                    // path: a per-sequence byte deadline.
                    if !self.dropped {
                        if self.buf.len() >= MAX_SEQ {
                            self.dropped = true;
                            self.dropped_more = k == Kind::Apc && more_chunks(&self.buf);
                            // Not `clear()`: release the 4MB, the pane is spewing.
                            self.buf = Vec::new();
                        } else {
                            self.buf.push(b);
                        }
                    }
                    if b == ESC {
                        self.esc = true;
                        continue;
                    }
                    let done = (self.esc && b == b'\\') || (k == Kind::Osc && b == BEL);
                    self.esc = false;
                    if done {
                        let seq = std::mem::take(&mut self.buf);
                        let dropped = std::mem::take(&mut self.dropped);
                        if k != Kind::Apc {
                            if !dropped {
                                out.push(Piece::Image(seq));
                            }
                        } else if dropped || self.chunks_dropped {
                            self.chunks_dropped = if dropped {
                                self.dropped_more
                            } else {
                                more_chunks(&seq)
                            };
                            self.chunks = Vec::new();
                        } else if more_chunks(&seq) {
                            if self.chunks.len() + seq.len() > MAX_IMAGE {
                                self.chunks = Vec::new();
                                self.chunks_dropped = true;
                            } else {
                                self.chunks.extend_from_slice(&seq);
                            }
                        } else if self.chunks.is_empty() {
                            out.push(Piece::Image(seq));
                        } else {
                            self.chunks.extend_from_slice(&seq);
                            out.push(Piece::Image(std::mem::take(&mut self.chunks)));
                        }
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
    fn only_sequences_that_draw_are_safe() {
        assert!(is_safe(
            b"\x1b_Ga=T,f=100,m=1;AAAA\x1b\\\x1b_Gm=0;AAAA\x1b\\"
        ));
        assert!(is_safe(b"\x1b_Ga=T,t=d;AAAA\x1b\\"));
        assert!(!is_safe(b"\x1b_Ga=T,t=f;L2V0Yy9wYXNzd2Q=\x1b\\"));
        assert!(!is_safe(b"\x1b_Ga=T,m=1;AA\x1b\\\x1b_Gt=s,m=0;AA\x1b\\"));
        assert!(is_safe(b"\x1b]1337;File=name=eA==;inline=1:AAAA\x07"));
        assert!(!is_safe(b"\x1b]1337;File=name=eA==:AAAA\x07"));
        assert!(!is_safe(b"\x1b]1337;StealFocus\x07"));
        assert!(is_safe(b"\x1bPq#0;2;0;0;0#0~~\x1b\\"));
    }

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
    fn a_chunked_kitty_image_is_one_image() {
        // mpv's `--vo=kitty` frame: a header chunk, middle chunks, a last one.
        let chunks: [&[u8]; 3] = [
            b"\x1b_Ga=T,f=24,s=2,v=1,m=1;AAAA\x1b\\",
            b"\x1b_Gm=1;BBBB\x1b\\",
            b"\x1b_Gm=0;CC\x1b\\",
        ];
        let whole = chunks.concat();
        let (plain, imgs) = run(&[&whole[..10], &whole[10..], b"x"]);
        assert_eq!(plain, b"x");
        assert_eq!(imgs, vec![whole.clone()]);

        // Too big to keep: the whole image goes, not its tail, and the
        // scanner is ready for the next one.
        let mut big = b"\x1b_Ga=T,m=1;".to_vec();
        big.resize(MAX_SEQ - 10, b'A');
        big.extend_from_slice(b"\x1b\\");
        let mut input = vec![];
        for _ in 0..MAX_IMAGE / big.len() + 1 {
            input.extend_from_slice(&big);
        }
        input.extend_from_slice(chunks[2]);
        input.extend_from_slice(KITTY);
        assert_eq!(run(&[&input]).1, vec![KITTY.to_vec()]);
    }

    #[test]
    fn an_escape_inside_a_sequence_cancels_it() {
        // A status line written into the middle of a chunked image: the
        // image is abandoned, the status line reaches vt100 without the
        // chunk's tail, and the chunks still to come are not replayed
        // without their start.
        let input: &[u8] = b"\x1b_Ga=T,m=1;AA\x1b\\\x1b_Gm=1;BB\x1b[1K\rV: 00:01 (72%)\
                             CC//\x1b\\\x1b_Gm=0;DD\x1b\\x";
        let (plain, imgs) = run(&[input]);
        assert!(imgs.is_empty(), "{imgs:?}");
        assert_eq!(plain, b"\x1b[1K\rV: 00:01 (72%)x");

        // Any other cut sequence is just cancelled, as in a terminal.
        let other: &[u8] = b"\x1b_Ga=T;AA\x1b[mCC\x1b\\";
        assert_eq!(run(&[other]), (other.to_vec(), vec![]));
        // The same bytes one at a time, then a good image straight after.
        let mut bytes: Vec<&[u8]> = input.chunks(1).collect();
        bytes.push(KITTY);
        assert_eq!(run(&bytes), (plain, vec![KITTY.to_vec()]));
    }

    #[test]
    fn iterm_also_ends_at_st() {
        let (plain, imgs) = run(&[b"\x1b]1337;File=inline=1:AA\x1b\\x"]);
        assert_eq!(plain, b"x");
        assert_eq!(imgs, vec![b"\x1b]1337;File=inline=1:AA\x1b\\".to_vec()]);
    }

    #[test]
    fn a_sequence_survives_being_split_across_reads() {
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

    /// The property the whole module rests on: whatever the scanner emits,
    /// plain and image pieces concatenated, is the input back byte for byte,
    /// and anything not yet emitted is still sitting in a bounded `buf`.
    #[test]
    fn random_input_is_never_lost_invented_or_buffered_without_bound() {
        // Alphabet biased to the bytes that steer the scanner; cuts are random
        // so every split point gets exercised.
        let alpha = b"\x1b_GP]q1337;\\\x07ab$+0";
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let mut rng = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as usize
        };
        for trial in 0..4000usize {
            let mut s = Scanner::new();
            let input: Vec<u8> = (0..1 + trial % 300)
                .map(|_| alpha[rng() % alpha.len()])
                .collect();
            let mut seen = Vec::new();
            let mut cut = 0;
            while cut < input.len() {
                let end = (cut + 1 + rng() % 7).min(input.len());
                for p in s.feed(&input[cut..end]) {
                    match p {
                        Piece::Plain(b) => seen.extend_from_slice(&b),
                        Piece::Image(b) => seen.extend_from_slice(&b),
                    }
                }
                assert!(s.buf.len() <= MAX_SEQ, "trial {trial}");
                cut = end;
            }
            assert_eq!(seen, input[..seen.len()], "trial {trial}");
            assert!(input.len() - seen.len() <= s.buf.len() + 1, "trial {trial}");
        }
    }
}
