//! Offline asciicast v2 -> .mov renderer that looks like Ghostty (Argonaut, Iosevka Term).
use ab_glyph::{point, Font, FontVec, PxScale, ScaleFont};
use ab_glyph_rasterizer::Rasterizer;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

const FFMPEG: &str = "/opt/homebrew/bin/ffmpeg";

// ---------------------------------------------------------------- colours (Argonaut)
const BG: [u8; 3] = [0x0e, 0x10, 0x19];
const FG: [u8; 3] = [0xff, 0xfa, 0xf4];
const PALETTE16: [u32; 16] = [
    0x232323, 0xff000f, 0x8ce10b, 0xffb900, 0x008df8, 0x6d43a6, 0x00d8eb, 0xffffff, 0x444444,
    0xff2740, 0xabe15b, 0xffd242, 0x0092ff, 0x9a5feb, 0x67fff0, 0xffffff,
];

fn idx_color(i: u8) -> [u8; 3] {
    match i {
        0..=15 => {
            let v = PALETTE16[i as usize];
            [(v >> 16) as u8, (v >> 8) as u8, v as u8]
        }
        16..=231 => {
            let i = i - 16;
            let l = |n: u8| if n == 0 { 0 } else { 55 + 40 * n };
            [l(i / 36), l((i / 6) % 6), l(i % 6)]
        }
        _ => {
            let g = 8 + 10 * (i - 232);
            [g, g, g]
        }
    }
}

fn color(c: vt100::Color, default: [u8; 3]) -> [u8; 3] {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => idx_color(i),
        vt100::Color::Rgb(r, g, b) => [r, g, b],
    }
}

fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    [f(a[0], b[0]), f(a[1], b[1]), f(a[2], b[2])]
}

// ---------------------------------------------------------------- cast
enum Ev {
    Out(String),
    Mouse(Option<(f32, f32, bool)>),
    Key(String),
}

fn load_cast(path: &str) -> (u16, u16, Vec<(f64, Ev)>) {
    let f = std::fs::File::open(path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
    let mut lines = std::io::BufReader::new(f).lines();
    let hdr: serde_json::Value =
        serde_json::from_str(&lines.next().expect("empty cast").unwrap()).expect("bad header");
    let cols = hdr["width"].as_u64().expect("width") as u16;
    let rows = hdr["height"].as_u64().expect("height") as u16;
    let mut evs = Vec::new();
    for line in lines {
        let line = line.unwrap();
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(&line).expect("bad event line");
        let (t, code, data) = (v[0].as_f64().unwrap_or(0.0), v[1].as_str().unwrap_or(""), v[2].as_str().unwrap_or(""));
        match code {
            "o" => evs.push((t, Ev::Out(data.to_string()))),
            "k" => evs.push((t, Ev::Key(data.to_string()))),
            "m" if data == "hide" => evs.push((t, Ev::Mouse(None))),
            "m" => {
                let p: Vec<&str> = data.split(',').collect();
                if p.len() >= 2 {
                    let x = p[0].trim().parse().unwrap_or(0.0);
                    let y = p[1].trim().parse().unwrap_or(0.0);
                    let down = p.get(2).map(|s| s.trim() == "down").unwrap_or(false);
                    evs.push((t, Ev::Mouse(Some((x, y, down)))));
                }
            }
            _ => {}
        }
    }
    evs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap()); // stable: keeps order of equal t
    (cols, rows, evs)
}

// ---------------------------------------------------------------- emulator + sync tracking
struct Emu {
    parser: vt100::Parser,
    evs: Vec<(f64, Ev)>,
    next: usize,
    matched: usize, // bytes of "\x1b[?2026" matched so far
    in_sync: bool,
    sync_since: f64,
    snapshot: Option<vt100::Screen>,
    pointer: Option<(f32, f32, bool)>,
    keys: Vec<String>, // labels in the current keypress badge
    last_key: f64,
    changed: bool,
}

// Keypress badge timing (seconds).
const KEY_JOIN: f64 = 1.0;
const KEY_HOLD: f64 = 1.5;
const KEY_FADE: f64 = 0.3;

const SYNC_PREFIX: &[u8] = b"\x1b[?2026";

impl Emu {
    fn advance_to(&mut self, t: f64) {
        while self.next < self.evs.len() && self.evs[self.next].0 <= t {
            let et = self.evs[self.next].0;
            match std::mem::replace(&mut self.evs[self.next].1, Ev::Mouse(None)) {
                Ev::Out(s) => self.feed(s.as_bytes(), et),
                Ev::Mouse(p) => self.pointer = p,
                Ev::Key(k) => {
                    if et - self.last_key > KEY_JOIN {
                        self.keys.clear();
                    }
                    self.keys.push(k);
                    self.last_key = et;
                }
            }
            self.next += 1;
            self.changed = true;
        }
    }

    fn feed(&mut self, b: &[u8], t: f64) {
        let mut seg = 0;
        for i in 0..b.len() {
            let c = b[i];
            if self.matched == SYNC_PREFIX.len() {
                self.matched = 0;
                if c == b'h' || c == b'l' {
                    self.parser.process(&b[seg..=i]);
                    seg = i + 1;
                    if c == b'h' && !self.in_sync {
                        // The escape itself changes nothing visible, so this is the
                        // last state outside the sync block.
                        self.snapshot = Some(self.parser.screen().clone());
                        self.in_sync = true;
                        self.sync_since = t;
                    } else if c == b'l' {
                        self.in_sync = false;
                        self.snapshot = None;
                    }
                    continue;
                }
            }
            if c == SYNC_PREFIX[self.matched] {
                self.matched += 1;
            } else {
                self.matched = if c == 0x1b { 1 } else { 0 };
            }
        }
        self.parser.process(&b[seg..]);
    }

    /// The keypress badge at time t: (labels, opacity).
    fn badge(&self, t: f64) -> Option<(&[String], f32)> {
        let age = t - self.last_key;
        if self.keys.is_empty() || age >= KEY_HOLD + KEY_FADE {
            return None;
        }
        let a = if age <= KEY_HOLD { 1.0 } else { 1.0 - (age - KEY_HOLD) / KEY_FADE };
        Some((&self.keys, a as f32))
    }

    fn screen(&self, t: f64) -> &vt100::Screen {
        // Real terminals give up on a sync block after ~1s; do the same.
        match &self.snapshot {
            Some(s) if self.in_sync && t - self.sync_since < 1.0 => s,
            _ => self.parser.screen(),
        }
    }
}

// ---------------------------------------------------------------- fonts & glyphs
struct Glyph {
    ox: i32,
    oy: i32,
    w: usize,
    h: usize,
    a: Vec<u8>,
}

struct Fonts {
    faces: [FontVec; 4], // regular, bold, italic, bold italic
    fallback: Vec<FontVec>,
    keysym: Option<FontVec>, // SF, for the badge's modifier symbols (Iosevka's are small)
    size: f32, // em size in px
    cw: usize,
    ch: usize,
    baseline: f32,
    line: usize, // box-drawing light line thickness
}

fn load_font(path: &str, index: u32) -> Option<FontVec> {
    let data = std::fs::read(path).ok()?;
    FontVec::try_from_vec_and_index(data, index).ok()
}

fn first_font(cands: &[String]) -> FontVec {
    cands.iter().find_map(|p| load_font(p, 0)).unwrap_or_else(|| die(&format!("no font among {cands:?}")))
}

fn pxscale(f: &FontVec, size: f32) -> PxScale {
    PxScale::from(size * f.height_unscaled() / f.units_per_em().unwrap_or(1000.0))
}

impl Fonts {
    fn new(cols: u16, font_dir_override: Option<f32>) -> Fonts {
        let home = std::env::var("HOME").unwrap_or_default();
        let find = |names: &[&str]| -> Vec<String> {
            let mut v = Vec::new();
            for n in names {
                v.push(format!("{home}/Library/Fonts/{n}"));
                v.push(format!("/Library/Fonts/{n}"));
            }
            v
        };
        let regular = first_font(&find(&["iosevka-term-medium.ttf", "iosevka-term-regular.ttf"]));
        let bold = first_font(&find(&["iosevka-term-bold.ttf", "iosevka-term-medium.ttf", "iosevka-term-regular.ttf"]));
        let italic = first_font(&find(&["iosevka-term-mediumitalic.ttf", "iosevka-term-italic.ttf", "iosevka-term-medium.ttf"]));
        let bolditalic = first_font(&find(&["iosevka-term-bolditalic.ttf", "iosevka-term-mediumitalic.ttf", "iosevka-term-medium.ttf"]));
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.to_path_buf()));
        let mut fallback = Vec::new();
        let mut nerd = vec![format!("{home}/Library/Fonts/SymbolsNerdFontMono-Regular.ttf")];
        if let Some(d) = &exe_dir {
            for up in [d.clone(), d.join(".."), d.join("../.."), d.join("../../..")] {
                nerd.push(up.join("fonts/SymbolsNerdFontMono-Regular.ttf").to_string_lossy().into());
            }
        }
        if let Some(f) = nerd.iter().find_map(|p| load_font(p, 0)) {
            fallback.push(f);
        } else {
            eprintln!("warning: Symbols Nerd Font Mono not found");
        }
        for (p, i) in [
            ("/System/Library/Fonts/Apple Symbols.ttf", 0),
            ("/System/Library/Fonts/Menlo.ttc", 0),
            ("/System/Library/Fonts/Supplemental/Arial Unicode.ttf", 0),
        ] {
            if let Some(f) = load_font(p, i) {
                fallback.push(f);
            }
        }
        let upem = regular.units_per_em().unwrap_or(1000.0);
        let adv = regular.h_advance_unscaled(regular.glyph_id('M')) / upem;
        let size = font_dir_override.unwrap_or_else(|| (1960.0 / cols as f32).round().max(6.0) / adv);
        let cw = (adv * size).round() as usize;
        let k = size / upem;
        let ch = ((regular.ascent_unscaled() - regular.descent_unscaled() + regular.line_gap_unscaled()) * k).round() as usize;
        let gap = regular.line_gap_unscaled() * k;
        let baseline = (gap / 2.0 + regular.ascent_unscaled() * k).round();
        let line = ((size * 0.07).round() as usize).max(1);
        let keysym = load_font("/System/Library/Fonts/SFNS.ttf", 0);
        Fonts { faces: [regular, bold, italic, bolditalic], fallback, keysym, size, cw, ch, baseline, line }
    }

    /// Proportional label text at 2x the cell font: glyph (origin at text box top-left) and advance width.
    fn label(&self, labels: &[String]) -> (Glyph, f32) {
        let size = 2.0 * self.size;
        let base = 2.0 * self.baseline;
        let gap = 0.3 * size; // space between joined labels
        let total: f32 = labels.iter().flat_map(|l| l.chars()).count() as f32 * size + gap * labels.len() as f32;
        let (w, h) = (total.ceil() as usize + 8, 2 * self.ch + 8);
        let mut buf = vec![0f32; w * h];
        let mut pen = 0.0f32;
        for (n, l) in labels.iter().enumerate() {
            if n > 0 {
                pen += gap;
            }
            for c in l.chars() {
                let modifier = "⌃⌥⇧⌘⎋⇥⌫⌦".contains(c);
                let font = self.keysym.iter().filter(|_| modifier).chain(std::iter::once(&self.faces[0])).chain(&self.fallback).find(|f| f.glyph_id(c).0 != 0).unwrap_or(&self.faces[0]);
                let gid = font.glyph_id(c);
                let sc = pxscale(font, size);
                if let Some(o) = font.outline_glyph(gid.with_scale_and_position(sc, point(pen, base))) {
                    let b = o.px_bounds();
                    o.draw(|x, y, v| {
                        let (x, y) = (b.min.x as i32 + x as i32 + 4, b.min.y as i32 + y as i32 + 4);
                        if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                            let p = &mut buf[y as usize * w + x as usize];
                            *p = (*p + v).min(1.0);
                        }
                    });
                }
                pen += font.as_scaled(sc).h_advance(gid);
            }
        }
        (trim(&buf, w, h, -4, -4), pen)
    }

    fn rasterize(&self, text: &str, style: usize, cells: usize) -> Glyph {
        let c = text.chars().next().unwrap_or(' ');
        let (cw, ch) = (self.cw, self.ch);
        let span = cw * cells;
        if let Some(g) = procedural(c, span, ch, self.line) {
            return g;
        }
        // Pick a face that has the glyph.
        let primary = [&self.faces[style], &self.faces[0]];
        let (font, is_primary) = match primary.iter().find(|f| f.glyph_id(c).0 != 0) {
            Some(f) => (*f, true),
            None => match self.fallback.iter().find(|f| f.glyph_id(c).0 != 0) {
                Some(f) => (f, false),
                None => return empty(),
            },
        };
        // Canvas generously around the cell span, trimmed afterwards.
        let (pad_x, pad_y) = (cw as i32, ch as i32 / 2);
        let (w, h) = (span + 2 * cw, 2 * ch);
        let mut buf = vec![0f32; w * h];
        let mut put = |x: i32, y: i32, v: f32| {
            let (x, y) = (x + pad_x, y + pad_y);
            if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                let p = &mut buf[y as usize * w + x as usize];
                *p = (*p + v).min(1.0);
            }
        };
        let gid = font.glyph_id(c);
        let scale = pxscale(font, self.size);
        let powerline = (0xE0B0..=0xE0BF).contains(&(c as u32));
        let outline = |scale: PxScale, pos: (f32, f32)| font.outline_glyph(gid.with_scale_and_position(scale, point(pos.0, pos.1)));
        let Some(og) = outline(scale, (0.0, self.baseline)) else { return empty() };
        let b = og.px_bounds();
        let (bw, bh) = (b.width(), b.height());
        let og = if is_primary {
            og
        } else if powerline {
            // Stretch to fill the cell exactly, like Ghostty's powerline constraint.
            let s2 = PxScale { x: scale.x * span as f32 / bw, y: scale.y * ch as f32 / bh };
            let o = outline(s2, (0.0, self.baseline)).unwrap();
            let b2 = o.px_bounds();
            outline(s2, (-b2.min.x, self.baseline - b2.min.y)).unwrap()
        } else {
            // Fallback symbol: shrink to fit the cell span, centre horizontally,
            // centre vertically only when it had to shrink.
            // Nerd Font icons (private use area) are sized up to the cell like Ghostty does.
            let f = if is_icon(c) {
                (span as f32 * 0.92 / bw).min(ch as f32 * 0.62 / bh)
            } else {
                (span as f32 / bw).min(ch as f32 / bh).min(1.0)
            };
            let s2 = PxScale { x: scale.x * f, y: scale.y * f };
            let o = outline(s2, (0.0, self.baseline)).unwrap();
            let b2 = o.px_bounds();
            let dx = (span as f32 - b2.width()) / 2.0 - b2.min.x;
            let dy = if f < 1.0 || is_icon(c) { (ch as f32 - b2.height()) / 2.0 - b2.min.y } else { 0.0 };
            outline(s2, (dx.round(), self.baseline + dy.round())).unwrap()
        };
        let b = og.px_bounds();
        og.draw(|x, y, v| put(b.min.x as i32 + x as i32, b.min.y as i32 + y as i32, v));
        // Combining marks: draw at the base glyph's advance.
        let adv = font.as_scaled(scale).h_advance(gid);
        for m in text.chars().skip(1) {
            let u = m as u32;
            let combining = (0x300..=0x36F).contains(&u) || (0x1AB0..=0x1AFF).contains(&u) || (0x1DC0..=0x1DFF).contains(&u) || (0x20D0..=0x20FF).contains(&u) || (0xFE20..=0xFE2F).contains(&u);
            if !combining {
                continue;
            }
            let f = if font.glyph_id(m).0 != 0 { font } else { &self.faces[0] };
            if let Some(o) = f.outline_glyph(f.glyph_id(m).with_scale_and_position(pxscale(f, self.size), point(adv, self.baseline))) {
                let b = o.px_bounds();
                o.draw(|x, y, v| put(b.min.x as i32 + x as i32, b.min.y as i32 + y as i32, v));
            }
        }
        trim(&buf, w, h, -pad_x, -pad_y)
    }
}

fn is_icon(c: char) -> bool {
    let u = c as u32;
    ((0xE000..=0xF8FF).contains(&u) && !(0xE0A0..=0xE0BF).contains(&u)) || u >= 0xF0000
}

fn empty() -> Glyph {
    Glyph { ox: 0, oy: 0, w: 0, h: 0, a: vec![] }
}

fn trim(buf: &[f32], w: usize, h: usize, ox: i32, oy: i32) -> Glyph {
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for y in 0..h {
        for x in 0..w {
            if buf[y * w + x] > 0.002 {
                x0 = x0.min(x);
                x1 = x1.max(x + 1);
                y0 = y0.min(y);
                y1 = y1.max(y + 1);
            }
        }
    }
    if x0 >= x1 {
        return empty();
    }
    let (gw, gh) = (x1 - x0, y1 - y0);
    let mut a = Vec::with_capacity(gw * gh);
    for y in y0..y1 {
        for x in x0..x1 {
            a.push((buf[y * w + x].clamp(0.0, 1.0) * 255.0).round() as u8);
        }
    }
    Glyph { ox: ox + x0 as i32, oy: oy + y0 as i32, w: gw, h: gh, a }
}

// ---------------------------------------------------------------- procedural glyphs
/// Box drawing arms (up, right, down, left): 0 none, 1 light, 2 heavy, 3 double.
const BOX: [&str; 76] = [
    "0101", "0202", "1010", "2020", "", "", "", "", "", "", "", "", // 2500-250B
    "0110", "0210", "0120", "0220", "0011", "0012", "0021", "0022", // 250C-2513
    "1100", "1200", "2100", "2200", "1001", "1002", "2001", "2002", // 2514-251B
    "1110", "1210", "2110", "1120", "2120", "2210", "1220", "2220", // 251C-2523
    "1011", "1012", "2011", "1021", "2021", "2012", "1022", "2022", // 2524-252B
    "0111", "0112", "0211", "0212", "0121", "0122", "0221", "0222", // 252C-2533
    "1101", "1102", "1201", "1202", "2101", "2102", "2201", "2202", // 2534-253B
    "1111", "1112", "1211", "1212", "2111", "1121", "2121", "2112", // 253C-2543
    "2211", "1122", "1221", "2212", "1222", "2122", "2221", "2222", // 2544-254B
];
const BOX_DOUBLE: [&str; 29] = [
    "0303", "3030", "0310", "0130", "0330", "0013", "0031", "0033", "1300", "3100", "3300", "1003",
    "3001", "3003", "1310", "3130", "3330", "1013", "3031", "3033", "0313", "0131", "0333", "1303",
    "3101", "3303", "1313", "3131", "3333",
];
const BOX_HALF: [&str; 12] = ["0001", "1000", "0100", "0010", "0002", "2000", "0200", "0020", "0201", "1020", "0102", "2010"];

struct Canvas {
    w: usize,
    h: usize,
    a: Vec<f32>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Canvas {
        Canvas { w, h, a: vec![0.0; w * h] }
    }
    fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, v: f32) {
        for y in y0.max(0)..y1.min(self.h as i32) {
            for x in x0.max(0)..x1.min(self.w as i32) {
                let p = &mut self.a[y as usize * self.w + x as usize];
                *p = p.max(v);
            }
        }
    }
    /// Anti-aliased fill from a per-pixel-centre coverage function.
    fn shade(&mut self, f: impl Fn(f32, f32) -> f32) {
        for y in 0..self.h {
            for x in 0..self.w {
                let v = f(x as f32 + 0.5, y as f32 + 0.5).clamp(0.0, 1.0);
                let p = &mut self.a[y * self.w + x];
                *p = p.max(v);
            }
        }
    }
    fn glyph(self) -> Glyph {
        trim(&self.a, self.w, self.h, 0, 0)
    }
}

fn procedural(c: char, cw: usize, ch: usize, t: usize) -> Option<Glyph> {
    let u = c as u32;
    let (w, h) = (cw as i32, ch as i32);
    let ti = t as i32;
    let mut cv = Canvas::new(cw, ch);
    // Light line placement shared by every box glyph so they join exactly.
    let x0 = (w - ti) / 2;
    let y0 = (h - ti) / 2;
    let (xm, ym) = (x0 as f32 + t as f32 / 2.0, y0 as f32 + t as f32 / 2.0);
    let arms = match u {
        0x2500..=0x254B => BOX[(u - 0x2500) as usize],
        0x2550..=0x256C => BOX_DOUBLE[(u - 0x2550) as usize],
        0x2574..=0x257F => BOX_HALF[(u - 0x2574) as usize],
        _ => "",
    };
    if !arms.is_empty() {
        let a: Vec<i32> = arms.bytes().map(|b| (b - b'0') as i32).collect();
        let thick = |k: i32| if k == 2 { 2 * ti } else { ti };
        // Arms end exactly at the far edge of the perpendicular line so joins are flush.
        let span = |k: i32, size: i32| match k {
            3 => ((size - ti) / 2 - ti, (size - ti) / 2 + 2 * ti),
            _ => ((size - thick(k)) / 2, (size - thick(k)) / 2 + thick(k)),
        };
        let pick = |p: i32, q: i32, own: i32| if p.max(q) > 0 { p.max(q) } else { own };
        let (va, vb) = span(pick(a[0], a[2], a[1].max(a[3])), w);
        let (ha, hb) = span(pick(a[1], a[3], a[0].max(a[2])), h);
        let hline = |cv: &mut Canvas, k: i32, xa: i32, xb: i32| {
            if k == 3 {
                cv.rect(xa, y0 - ti, xb, y0, 1.0);
                cv.rect(xa, y0 + ti, xb, y0 + 2 * ti, 1.0);
            } else if k > 0 {
                let tk = thick(k);
                let yy = (h - tk) / 2;
                cv.rect(xa, yy, xb, yy + tk, 1.0);
            }
        };
        let vline = |cv: &mut Canvas, k: i32, ya: i32, yb: i32| {
            if k == 3 {
                cv.rect(x0 - ti, ya, x0, yb, 1.0);
                cv.rect(x0 + ti, ya, x0 + 2 * ti, yb, 1.0);
            } else if k > 0 {
                let tk = thick(k);
                let xx = (w - tk) / 2;
                cv.rect(xx, ya, xx + tk, yb, 1.0);
            }
        };
        vline(&mut cv, a[0], 0, hb);
        hline(&mut cv, a[1], va, w);
        vline(&mut cv, a[2], ha, h);
        hline(&mut cv, a[3], 0, vb);
        return Some(cv.glyph());
    }
    let tf = t as f32;
    let line_cov = move |d: f32| tf / 2.0 + 0.5 - d;
    match u {
        // dashed lines
        0x2504..=0x250B | 0x254C..=0x254F => {
            let (horiz, heavy, n) = match u {
                0x2504 => (true, false, 3), 0x2505 => (true, true, 3), 0x2506 => (false, false, 3), 0x2507 => (false, true, 3),
                0x2508 => (true, false, 4), 0x2509 => (true, true, 4), 0x250A => (false, false, 4), 0x250B => (false, true, 4),
                0x254C => (true, false, 2), 0x254D => (true, true, 2), 0x254E => (false, false, 2), _ => (false, true, 2),
            };
            let tk = if heavy { 2 * ti } else { ti };
            let len = if horiz { w } else { h };
            for i in 0..n {
                let a = len * i / n;
                let b = len * (i + 1) / n;
                let gap = ((b - a) / 3).max(1);
                let (a, b) = (a + gap / 2, b - (gap - gap / 2));
                if horiz {
                    cv.rect(a, (h - tk) / 2, b, (h - tk) / 2 + tk, 1.0);
                } else {
                    cv.rect((w - tk) / 2, a, (w - tk) / 2 + tk, b, 1.0);
                }
            }
        }
        // rounded corners
        0x256D..=0x2570 => {
            let (sx, sy) = match u {
                0x256D => (1.0, 1.0),
                0x256E => (-1.0, 1.0),
                0x256F => (-1.0, -1.0),
                _ => (1.0, -1.0),
            };
            let r = (cw.min(ch) as f32 / 2.0).max(tf);
            cv.shade(|px, py| {
                let lx = (px - xm) * sx;
                let ly = (py - ym) * sy;
                let d = if lx < r && ly < r {
                    ((lx - r).hypot(ly - r) - r).abs()
                } else if lx >= r {
                    ly.abs()
                } else {
                    lx.abs()
                };
                line_cov(d)
            });
        }
        0x2571..=0x2573 => {
            let (wf, hf) = (cw as f32, ch as f32);
            let len = wf.hypot(hf);
            cv.shade(|px, py| {
                let d1 = ((px * hf + py * wf - wf * hf) / len).abs(); // ╱
                let d2 = ((px * hf - py * wf) / len).abs(); // ╲
                let d = match u {
                    0x2571 => d1,
                    0x2572 => d2,
                    _ => d1.min(d2),
                };
                line_cov(d)
            });
        }
        // block elements
        0x2580..=0x259F => {
            let yb = |k: i32| (h as f32 * k as f32 / 8.0).round() as i32;
            let xb = |k: i32| (w as f32 * k as f32 / 8.0).round() as i32;
            let quad = |cv: &mut Canvas, m: u8| {
                let (xh, yh) = (xb(4), yb(4));
                if m & 1 != 0 { cv.rect(0, 0, xh, yh, 1.0) }
                if m & 2 != 0 { cv.rect(xh, 0, w, yh, 1.0) }
                if m & 4 != 0 { cv.rect(0, yh, xh, h, 1.0) }
                if m & 8 != 0 { cv.rect(xh, yh, w, h, 1.0) }
            };
            match u {
                0x2580 => cv.rect(0, 0, w, yb(4), 1.0),
                0x2581..=0x2587 => cv.rect(0, yb(8 - (u - 0x2580) as i32), w, h, 1.0),
                0x2588 => cv.rect(0, 0, w, h, 1.0),
                0x2589..=0x258F => cv.rect(0, 0, xb(8 - (u - 0x2588) as i32), h, 1.0),
                0x2590 => cv.rect(xb(4), 0, w, h, 1.0),
                0x2591 => cv.rect(0, 0, w, h, 0.25),
                0x2592 => cv.rect(0, 0, w, h, 0.5),
                0x2593 => cv.rect(0, 0, w, h, 0.75),
                0x2594 => cv.rect(0, 0, w, yb(1), 1.0),
                0x2595 => cv.rect(xb(7), 0, w, h, 1.0),
                _ => quad(&mut cv, [4u8, 8, 1, 13, 9, 7, 11, 2, 6, 14][(u - 0x2596) as usize]),
            }
        }
        // braille
        0x2800..=0x28FF => {
            let bits = u - 0x2800;
            let r = (cw as f32 / 4.0).min(ch as f32 / 8.0) * 0.72;
            let dots = [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3)];
            cv.shade(|px, py| {
                let mut v: f32 = 0.0;
                for (i, (dx, dy)) in dots.iter().enumerate() {
                    if bits & (1 << i) != 0 {
                        let cx = cw as f32 * (1.0 + 2.0 * *dx as f32) / 4.0;
                        let cy = ch as f32 * (1.0 + 2.0 * *dy as f32) / 8.0;
                        v = v.max(r + 0.5 - (px - cx).hypot(py - cy));
                    }
                }
                v
            });
        }
        _ => return None,
    }
    Some(cv.glyph())
}

// ---------------------------------------------------------------- frame renderer
struct Renderer {
    fonts: Fonts,
    cache: HashMap<String, Glyph>,
    key: String,
    cols: usize,
    rows: usize,
    w: usize,
    h: usize,
    buf: Vec<u8>,
    badge_cache: Option<(String, Glyph, f32)>,
}

impl Renderer {
    fn fill(&mut self, x0: usize, y0: usize, x1: usize, y1: usize, c: [u8; 3]) {
        for y in y0..y1.min(self.h) {
            let row = &mut self.buf[(y * self.w + x0) * 3..(y * self.w + x1.min(self.w)) * 3];
            for px in row.chunks_exact_mut(3) {
                px.copy_from_slice(&c);
            }
        }
    }

    fn blend(&mut self, x: i32, y: i32, c: [u8; 3], a: f32) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h || a <= 0.0 {
            return;
        }
        let i = (y as usize * self.w + x as usize) * 3;
        let a = a.min(1.0);
        for k in 0..3 {
            let d = self.buf[i + k] as f32;
            self.buf[i + k] = (d + (c[k] as f32 - d) * a).round() as u8;
        }
    }

    fn draw_glyph(&mut self, text: &str, style: usize, cells: usize, x: usize, y: usize, fg: [u8; 3]) {
        self.key.clear();
        self.key.push((b'0' + style as u8 * 4 + cells as u8) as char);
        self.key.push_str(text);
        if !self.cache.contains_key(self.key.as_str()) {
            let g = self.fonts.rasterize(text, style, cells);
            self.cache.insert(self.key.clone(), g);
        }
        let g = &self.cache[self.key.as_str()];
        let (gx, gy) = (x as i32 + g.ox, y as i32 + g.oy);
        let (w, h) = (self.w as i32, self.h as i32);
        for j in 0..g.h {
            let py = gy + j as i32;
            if py < 0 || py >= h {
                continue;
            }
            for i in 0..g.w {
                let a = g.a[j * g.w + i];
                let px = gx + i as i32;
                if a == 0 || px < 0 || px >= w {
                    continue;
                }
                let o = (py as usize * self.w + px as usize) * 3;
                if a == 255 {
                    self.buf[o..o + 3].copy_from_slice(&fg);
                } else {
                    let a = a as u32;
                    for k in 0..3 {
                        let d = self.buf[o + k] as u32;
                        self.buf[o + k] = ((d * (255 - a) + fg[k] as u32 * a + 127) / 255) as u8;
                    }
                }
            }
        }
    }

    fn render(&mut self, screen: &vt100::Screen, pointer: Option<(f32, f32, bool)>, badge: Option<(&[String], f32)>) {
        let (cw, ch) = (self.fonts.cw, self.fonts.ch);
        let full_w = self.w;
        self.fill(0, 0, full_w, self.h, BG);
        let (crow, ccol) = screen.cursor_position();
        let show_cursor = !screen.hide_cursor();
        struct Item {
            x: usize,
            y: usize,
            cells: usize,
            gcells: usize,
            fg: [u8; 3],
            style: usize,
            text: String,
            ul: vt100::UnderlineStyle,
            ulc: [u8; 3],
            strike: bool,
        }
        let mut items = Vec::new();
        for r in 0..self.rows {
            for c in 0..self.cols {
                let Some(cell) = screen.cell(r as u16, c as u16) else { continue };
                if cell.is_wide_continuation() {
                    continue;
                }
                let cells = if cell.is_wide() && c + 1 < self.cols { 2 } else { 1 };
                // An icon followed by a blank cell may use both cells, as in Ghostty.
                let gcells = if cells == 1
                    && cell.contents().chars().next().is_some_and(is_icon)
                    && screen.cell(r as u16, c as u16 + 1).is_some_and(|n| matches!(n.contents(), "" | " "))
                {
                    2
                } else {
                    cells
                };
                let mut fgc = cell.fgcolor();
                if cell.bold() {
                    if let vt100::Color::Idx(i @ 0..=7) = fgc {
                        fgc = vt100::Color::Idx(i + 8);
                    }
                }
                let mut fg = color(fgc, FG);
                let mut bg = color(cell.bgcolor(), BG);
                if cell.inverse() {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if cell.dim() {
                    fg = mix(bg, fg, 0.5);
                }
                if cell.hidden() {
                    fg = bg;
                }
                if show_cursor && r as u16 == crow && c as u16 == ccol {
                    fg = [0, 0, 0];
                    bg = [0xff, 0xff, 0xff];
                }
                let (x, y) = (c * cw, r * ch);
                if bg != BG {
                    self.fill(x, y, x + cells * cw, y + ch, bg);
                }
                let ul = cell.underline_style();
                let ulc = color(cell.underline_color(), fg);
                if cell.has_contents() || ul != vt100::UnderlineStyle::None || cell.strikethrough() {
                    let style = cell.bold() as usize + 2 * cell.italic() as usize;
                    items.push(Item { x, y, cells, gcells, fg, style, text: cell.contents().to_string(), ul, ulc, strike: cell.strikethrough() });
                }
            }
        }
        // Text after all backgrounds so overhanging glyphs aren't clipped by neighbours.
        let lt = ((self.fonts.size / 14.0).round() as usize).max(1);
        let base = self.fonts.baseline as usize;
        for it in &items {
            if !it.text.is_empty() && it.text != " " {
                self.draw_glyph(&it.text, it.style, it.gcells, it.x, it.y, it.fg);
            }
            let wpx = it.cells * cw;
            let uy = (it.y + base + lt + 1).min(it.y + ch - lt);
            match it.ul {
                vt100::UnderlineStyle::None => {}
                vt100::UnderlineStyle::Single => self.fill(it.x, uy, it.x + wpx, uy + lt, it.ulc),
                vt100::UnderlineStyle::Double => {
                    let u0 = uy.saturating_sub(lt);
                    self.fill(it.x, u0, it.x + wpx, u0 + lt, it.ulc);
                    let u1 = (u0 + 2 * lt).min(it.y + ch - lt);
                    self.fill(it.x, u1, it.x + wpx, u1 + lt, it.ulc);
                }
                vt100::UnderlineStyle::Curly => {
                    let amp = (ch - base) as f32 / 2.0 - lt as f32 / 2.0;
                    let mid = (it.y + base) as f32 + (ch - base) as f32 / 2.0;
                    for px in 0..wpx {
                        let xf = px as f32 + 0.5;
                        let yc = mid + amp * (xf / cw as f32 * std::f32::consts::TAU).sin();
                        for py in (mid - amp - 2.0) as i32..(mid + amp + 3.0) as i32 {
                            let d = (py as f32 + 0.5 - yc).abs();
                            self.blend((it.x + px) as i32, py, it.ulc, lt as f32 / 2.0 + 0.5 - d);
                        }
                    }
                }
                _ => {
                    // dotted / dashed
                    let seg = if it.ul == vt100::UnderlineStyle::Dotted { lt.max(2) } else { cw / 2 };
                    let mut px = 0;
                    while px < wpx {
                        self.fill(it.x + px, uy, it.x + (px + seg).min(wpx), uy + lt, it.ulc);
                        px += 2 * seg;
                    }
                }
            }
            if it.strike {
                let sy = it.y + base - (self.fonts.size * 0.3).round() as usize;
                self.fill(it.x, sy, it.x + wpx, sy + lt, it.fg);
            }
        }
        if let Some((labels, a)) = badge {
            self.draw_badge(labels, a);
        }
        if let Some((x, y, down)) = pointer {
            self.draw_pointer((x + 0.5) * cw as f32, (y + 0.5) * ch as f32, down);
        }
    }

    /// KeyCastr-style badge: rounded dark translucent pill, centred above the bottom row.
    fn draw_badge(&mut self, labels: &[String], alpha: f32) {
        let key = labels.join("\u{1}");
        let (key, g, text_w) = match self.badge_cache.take() {
            Some(c) if c.0 == key => c,
            _ => {
                let (g, w) = self.fonts.label(labels);
                (key, g, w)
            }
        };
        let (cw, ch) = (self.fonts.cw as f32, self.fonts.ch as f32);
        let text_h = 2.0 * ch;
        let (pad_x, pad_y) = (1.6 * cw, 0.3 * ch);
        let (bw, bh) = ((text_w + 2.0 * pad_x).round(), (text_h + 2.0 * pad_y).round());
        let radius = 0.32 * bh;
        let bx = ((self.cols as f32 * cw - bw) / 2.0).round();
        let by = ((self.rows as f32 - 1.0) * ch - 1.0 * ch - bh).round(); // one row of air above the footer
        let border = (ch / 22.0).max(1.0);
        for y in by as i32 - 2..(by + bh) as i32 + 2 {
            for x in bx as i32 - 2..(bx + bw) as i32 + 2 {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                // Signed distance to the rounded rectangle.
                let qx = (px - (bx + bw / 2.0)).abs() - (bw / 2.0 - radius);
                let qy = (py - (by + bh / 2.0)).abs() - (bh / 2.0 - radius);
                let d = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius;
                let inside = (0.5 - d).clamp(0.0, 1.0);
                let ring = (0.5 - (d + border / 2.0).abs() + border / 2.0).clamp(0.0, 1.0);
                self.blend(x, y, [0x10, 0x11, 0x16], inside * 0.80 * alpha);
                self.blend(x, y, [0xff, 0xff, 0xff], ring * 0.16 * alpha);
            }
        }
        let (ox, oy) = (bx + pad_x, by + pad_y);
        for j in 0..g.h {
            for i in 0..g.w {
                let a = g.a[j * g.w + i];
                if a > 0 {
                    self.blend(ox as i32 + g.ox + i as i32, oy as i32 + g.oy + j as i32, [0xff, 0xff, 0xff], a as f32 / 255.0 * alpha);
                }
            }
        }
        self.badge_cache = Some((key, g, text_w));
    }

    fn draw_pointer(&mut self, tx: f32, ty: f32, down: bool) {
        let ch = self.fonts.ch as f32;
        if down {
            let r = ch * 0.8;
            let ring = (ch / 14.0).max(1.5);
            let (x0, x1) = ((tx - r - 2.0) as i32, (tx + r + 3.0) as i32);
            let (y0, y1) = ((ty - r - 2.0) as i32, (ty + r + 3.0) as i32);
            for y in y0..y1 {
                for x in x0..x1 {
                    let d = (x as f32 + 0.5 - tx).hypot(y as f32 + 0.5 - ty);
                    let inside = (r + 0.5 - d).clamp(0.0, 1.0);
                    let edge = (ring / 2.0 + 0.5 - (d - r + ring / 2.0).abs()).clamp(0.0, 1.0);
                    self.blend(x, y, [0xff, 0xff, 0xff], inside * 0.16);
                    self.blend(x, y, [0xff, 0xff, 0xff], edge * 0.55);
                }
            }
        }
        // Classic arrow, tip at origin, ~19 units tall.
        let arrow: [(f32, f32); 7] = [(0.0, 0.0), (0.0, 16.5), (3.9, 12.9), (6.5, 19.0), (9.0, 17.9), (6.6, 11.9), (11.8, 11.9)];
        let s = ch * 1.08 / 19.0;
        let pts: Vec<(f32, f32)> = arrow.iter().map(|&(x, y)| (x * s, y * s)).collect();
        let ow = (s * 0.95).max(1.2);
        let (ox, oy) = (tx.floor() as i32 - 8, ty.floor() as i32 - 8);
        let (fx, fy) = (tx - ox as f32, ty - oy as f32);
        let bw = (13.0 * s) as usize + 18;
        let bh = (20.0 * s) as usize + 18;
        let layer = |poly: &[(f32, f32)], dx: f32, dy: f32| {
            let mut r = Rasterizer::new(bw, bh);
            for i in 0..poly.len() {
                let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
                r.draw_line(point(a.0 + fx + dx, a.1 + fy + dy), point(b.0 + fx + dx, b.1 + fy + dy));
            }
            let mut out = vec![0f32; bw * bh];
            r.for_each_pixel_2d(|x, y, v| out[y as usize * bw + x as usize] = v.min(1.0));
            out
        };
        let shadow = layer(&offset_poly(&pts, ow + 1.5), 0.0, s * 1.2);
        let outline = layer(&offset_poly(&pts, ow), 0.0, 0.0);
        let fill = layer(&pts, 0.0, 0.0);
        for y in 0..bh {
            for x in 0..bw {
                let i = y * bw + x;
                let (px, py) = (ox + x as i32, oy + y as i32);
                self.blend(px, py, [0, 0, 0], shadow[i] * 0.28);
                self.blend(px, py, [0, 0, 0], outline[i]);
                self.blend(px, py, [0xff, 0xff, 0xff], fill[i]);
            }
        }
    }
}

/// Offset a simple polygon outward by d (miter joins).
fn offset_poly(p: &[(f32, f32)], d: f32) -> Vec<(f32, f32)> {
    let n = p.len();
    let area: f32 = (0..n).map(|i| p[i].0 * p[(i + 1) % n].1 - p[(i + 1) % n].0 * p[i].1).sum();
    let sgn = if area < 0.0 { 1.0 } else { -1.0 };
    let normal = |a: (f32, f32), b: (f32, f32)| {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let l = dx.hypot(dy);
        (sgn * -dy / l, sgn * dx / l)
    };
    (0..n)
        .map(|i| {
            let (a, b, c) = (p[(i + n - 1) % n], p[i], p[(i + 1) % n]);
            let (n1, n2) = (normal(a, b), normal(b, c));
            let k = 1.0 + n1.0 * n2.0 + n1.1 * n2.1;
            (b.0 + d * (n1.0 + n2.0) / k, b.1 + d * (n1.1 + n2.1) / k)
        })
        .collect()
}

// ---------------------------------------------------------------- main
fn die(msg: &str) -> ! {
    eprintln!("render: {msg}");
    std::process::exit(1)
}

const USAGE: &str = "usage: render <cast> <out.mov> [--fps 30] [--start S] [--end E] [--frames-dir DIR] [--font-size PX]\n       render <cast> --frame T <out.png> [--font-size PX]";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut pos = Vec::new();
    let (mut fps, mut start, mut end, mut frames_dir, mut still, mut font_size) = (30.0f64, 0.0f64, None, None, None, None);
    let mut i = 0;
    while i < args.len() {
        let val = |i: usize| args.get(i + 1).cloned().unwrap_or_else(|| die(USAGE));
        let num = |i: usize| val(i).parse::<f64>().unwrap_or_else(|_| die(USAGE));
        match args[i].as_str() {
            "--fps" => { fps = num(i); i += 1 }
            "--start" => { start = num(i); i += 1 }
            "--end" => { end = Some(num(i)); i += 1 }
            "--frames-dir" => { frames_dir = Some(val(i)); i += 1 }
            "--font-size" => { font_size = Some(num(i) as f32); i += 1 }
            "--frame" => { still = Some(num(i)); i += 1 }
            "-h" | "--help" => { println!("{USAGE}"); return }
            a => pos.push(a.to_string()),
        }
        i += 1;
    }
    if pos.len() != 2 {
        die(USAGE);
    }
    let (cols, rows, evs) = load_cast(&pos[0]);
    let last_t = evs.last().map(|e| e.0).unwrap_or(0.0);
    let fonts = Fonts::new(cols, font_size);
    let (cw, ch) = (fonts.cw, fonts.ch);
    let w = (cols as usize * cw + 1) & !1;
    let h = (rows as usize * ch + 1) & !1;
    let mut rd = Renderer { fonts, cache: HashMap::new(), key: String::new(), cols: cols as usize, rows: rows as usize, w, h, buf: vec![0; w * h * 3], badge_cache: None };
    let mut emu = Emu {
        parser: vt100::Parser::new(rows, cols, 0),
        evs,
        next: 0,
        matched: 0,
        in_sync: false,
        sync_since: 0.0,
        snapshot: None,
        pointer: None,
        keys: Vec::new(),
        last_key: f64::NEG_INFINITY,
        changed: true,
    };
    let size = format!("{w}x{h}");
    let base_args = ["-hide_banner", "-loglevel", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", &size];

    if let Some(t) = still {
        emu.advance_to(t);
        rd.render(emu.screen(t), emu.pointer, emu.badge(t));
        let mut ff = Command::new(FFMPEG).args(base_args).args(["-i", "-", "-frames:v", "1", &pos[1]]).stdin(Stdio::piped()).spawn().unwrap_or_else(|e| die(&format!("ffmpeg: {e}")));
        ff.stdin.take().unwrap().write_all(&rd.buf).unwrap();
        if !ff.wait().unwrap().success() {
            die("ffmpeg failed");
        }
        eprintln!("{w}x{h} (cell {cw}x{ch}) -> {}", pos[1]);
        return;
    }

    let end = end.unwrap_or(last_t + 1.0 / fps);
    let n = ((end - start) * fps).ceil().max(1.0) as usize;
    let fps_s = format!("{fps}");
    let mut cmd = Command::new(FFMPEG);
    cmd.args(base_args).args(["-framerate", &fps_s, "-i", "-"]);
    cmd.args([
        "-vf", "scale=out_color_matrix=bt709:out_range=tv,format=yuv420p",
        "-c:v", "libx264", "-preset", "slow", "-crf", "16", "-pix_fmt", "yuv420p",
        "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv",
        "-movflags", "+faststart", &pos[1],
    ]);
    if let Some(d) = &frames_dir {
        std::fs::create_dir_all(d).unwrap_or_else(|e| die(&format!("{d}: {e}")));
        cmd.arg(format!("{d}/frame_%06d.png"));
    }
    let mut ff = cmd.stdin(Stdio::piped()).spawn().unwrap_or_else(|e| die(&format!("ffmpeg: {e}")));
    let mut stdin = std::io::BufWriter::with_capacity(1 << 22, ff.stdin.take().unwrap());
    let t0 = std::time::Instant::now();
    let mut last_sync = None;
    for k in 0..n {
        let t = start + k as f64 / fps;
        emu.advance_to(t);
        let sync_now = emu.in_sync && t - emu.sync_since < 1.0;
        // A badge that is fading must be redrawn every frame.
        let fading = emu.badge(t).is_some_and(|(_, a)| a < 1.0) || emu.badge(t - 1.0 / fps).is_some_and(|(_, a)| a < 1.0);
        if emu.changed || fading || last_sync != Some(sync_now) {
            rd.render(emu.screen(t), emu.pointer, emu.badge(t));
            emu.changed = false;
            last_sync = Some(sync_now);
        }
        if stdin.write_all(&rd.buf).is_err() {
            break;
        }
        if k % 300 == 299 {
            eprintln!("frame {}/{n} ({:.1}s elapsed)", k + 1, t0.elapsed().as_secs_f64());
        }
    }
    drop(stdin);
    if !ff.wait().unwrap().success() {
        die("ffmpeg failed");
    }
    eprintln!("{n} frames {w}x{h} (cell {cw}x{ch}) in {:.1}s -> {}", t0.elapsed().as_secs_f64(), pos[1]);
}
