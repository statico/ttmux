//! The status rows.
//!
//! Each row is composed from the widget names listed in its [`Bar`]
//! (`left`/`center`/`right`), so users can build their own; colours and the
//! background effect are shared by both rows on [`StatusBar`]. [`draw`] paints
//! one row and returns the on-screen column range of each tab it rendered, so
//! the app can turn a click into a tab index.

use std::ops::Range;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::buffer::{Buffer, CellWidth};
use ratatui::style::{Color, Modifier, Style};

use crate::agent::AgentState;
use crate::config::{Bar, BarEffect, StatusBar};
use crate::layout::Rect;
use crate::render::put_cell;

/// Everything the bar can show. Owned by the app, borrowed for one frame.
pub struct Ctx<'a> {
    pub session: &'a str,
    pub mode: &'a str,
    pub zoomed: bool,
    /// (name, active)
    pub tabs: &'a [(String, bool)],
    pub panes: &'a [(String, AgentState)],
    pub alerts: usize,
    pub message: Option<&'a str>,
    pub pending_prefix: bool,
}

/// Every name [`widget`] knows. Any other name in a [`Bar`] renders as
/// `?name` rather than vanishing, so a typo in the config is visible.
///
/// Nothing validates a `Bar` against this list yet; it is the canonical set
/// for documentation and for the render-everything test.
pub const WIDGETS: &[&str] = &[
    "session", "mode", "tabs", "panes", "agents", "alerts", "time", "date", "host", "load",
    "battery", "zoom", "prefix", "message", "spacer",
];

const WARN: Color = Color::Yellow;
const OK: Color = Color::Green;

/// A styled run of text. `tab` marks a clickable tab, `expand` a spacer.
#[derive(Clone)]
struct Span {
    text: String,
    style: Style,
    tab: Option<usize>,
    expand: bool,
}

impl Span {
    fn new(text: impl Into<String>, style: Style) -> Span {
        Span {
            text: text.into(),
            style,
            tab: None,
            expand: false,
        }
    }
}

/// Columns `Buffer::set_stringn` will actually advance for `s`.
///
/// This must agree with ratatui's `CellWidth` exactly. Every hitbox handed
/// back is measured with it, so a disagreement of one column makes a click
/// land on a tab that was never painted there. Plain `UnicodeWidthStr` will
/// not do: ratatui drops control-containing graphemes (one column too wide)
/// and charges a column for a halfwidth dakuten (one column too narrow).
fn sw(s: &str) -> usize {
    // A grapheme containing a control char is made only of control chars, so
    // dropping the chars is the same as dropping the graphemes ratatui filters.
    if s.chars().any(char::is_control) {
        let stripped: String = s.chars().filter(|c| !c.is_control()).collect();
        return stripped.as_str().cell_width() as usize;
    }
    s.cell_width() as usize
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| sw(&s.text)).sum()
}

/// Render one row into `rect`; returns `(tab index, columns)` for each tab drawn.
pub fn draw(
    buf: &mut Buffer,
    rect: Rect,
    cfg: &StatusBar,
    bar: &Bar,
    ctx: &Ctx,
) -> Vec<(usize, Range<u16>)> {
    if rect.w == 0 || rect.h == 0 {
        return Vec::new();
    }
    let base = Style::default().fg(cfg.fg.0).bg(cfg.bg.0);
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            put_cell(buf, x, y, " ", base);
        }
    }

    let w = rect.w as usize;
    let sep = || Span::new(cfg.separator.clone(), base);
    let group = |names: &[String]| join(chunks(names, cfg, ctx, base), &sep);
    let mut left = group(&bar.left);
    let mut center_chunks = chunks(&bar.center, cfg, ctx, base);
    let right = group(&bar.right);

    // Too wide? Drop centre widgets first, then truncate the left group.
    let rw = spans_width(&right);
    let mut center = join(center_chunks.clone(), &sep);
    while spans_width(&left) + spans_width(&center) + rw > w && !center_chunks.is_empty() {
        center_chunks.pop();
        center = join(center_chunks.clone(), &sep);
    }
    let lw_budget = w.saturating_sub(rw);
    if let Some(active) = ctx.tabs.iter().position(|(_, a)| *a) {
        scroll_tabs(&mut left, active, lw_budget);
    }
    if spans_width(&left) > lw_budget {
        left = truncate(left, lw_budget);
    }

    let lw = spans_width(&left);
    let cw = spans_width(&center);
    let free = w.saturating_sub(lw + cw + rw);
    expand_spacers(&mut left, free);

    let lw = spans_width(&left);
    let gap = w.saturating_sub(lw + rw);
    let cw = spans_width(&center).min(gap);

    let mut hits = Vec::new();
    let mut taken = vec![false; w];
    let end = rect.right();
    let mut row = Row {
        buf: &mut *buf,
        hits: &mut hits,
        taken: &mut taken,
        x0: rect.x,
        y: rect.y,
        end,
    };
    row.put(rect.x, &left);
    row.put(rect.x + (lw + (gap - cw) / 2) as u16, &center);
    row.put(end - rw.min(w) as u16, &right);

    // The effect goes in the cells no widget claimed, so widget text, its
    // colours and the hitboxes are identical under every effect.
    paint_effect(buf, rect, cfg, &taken, now_secs());

    hits.sort_by_key(|(_, r)| r.start);
    hits
}

fn chunks(names: &[String], cfg: &StatusBar, ctx: &Ctx, base: Style) -> Vec<Vec<Span>> {
    names
        .iter()
        .map(|n| widget(n, cfg, ctx, base))
        .filter(|c| !c.is_empty())
        .collect()
}

fn join(chunks: Vec<Vec<Span>>, sep: &dyn Fn() -> Span) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    for c in chunks {
        if !out.is_empty() {
            out.push(sep());
        }
        out.extend(c);
    }
    out
}

/// Share `free` columns out between the `spacer` spans in one group. Only the
/// left group is ever given any: the centre is centred and the right hugs the
/// edge, so a spacer there has nothing to push against and stays zero-width.
fn expand_spacers(spans: &mut [Span], free: usize) {
    let n = spans.iter().filter(|s| s.expand).count();
    if n == 0 || free == 0 {
        return;
    }
    let each = free / n;
    let mut extra = free % n;
    for s in spans.iter_mut().filter(|s| s.expand) {
        let mut k = each;
        if extra > 0 {
            k += 1;
            extra -= 1;
        }
        s.text = " ".repeat(k);
    }
}

/// Scroll the tab run like tmux does: drop whole tabs off its left until the
/// active one fits in `max`, so it is never both invisible and unclickable.
/// (`truncate` only ever cuts from the right, so without this the active tab
/// simply falls off the end.)
fn scroll_tabs(spans: &mut Vec<Span>, active: usize, max: usize) {
    let Some(a) = spans.iter().position(|s| s.tab == Some(active)) else {
        return;
    };
    let head: usize = spans[..=a].iter().map(|s| sw(&s.text)).sum();
    let Some(mut need) = head.checked_sub(max) else {
        return;
    };
    // Tabs are one contiguous run (separators only go between widgets), so the
    // first tab span is the left edge of what may be scrolled away.
    let start = spans.iter().position(|s| s.tab.is_some()).unwrap_or(a);
    let mut drop = 0;
    while start + drop < a && need > 0 {
        need = need.saturating_sub(sw(&spans[start + drop].text));
        drop += 1;
    }
    spans.drain(start..start + drop);
}

/// Keep spans while they fit in `max` columns, ending with `…` if anything was cut.
fn truncate(spans: Vec<Span>, max: usize) -> Vec<Span> {
    if max == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut used = 0;
    for s in spans {
        let sew = sw(&s.text);
        if used + sew <= max {
            used += sew;
            out.push(s);
            continue;
        }
        // Partially fit this span, leaving one column for the ellipsis.
        let room = max - used;
        if room > 1 {
            let mut text = String::new();
            let mut tw = 0;
            for ch in s.text.chars() {
                let cw = sw(ch.encode_utf8(&mut [0u8; 4]));
                if tw + cw > room - 1 {
                    break;
                }
                tw += cw;
                text.push(ch);
            }
            text.push('…');
            out.push(Span { text, ..s });
        } else if room == 1 {
            out.push(Span::new("…", s.style));
        }
        break;
    }
    out
}

/// One row being painted: where the text goes, plus what it claimed.
struct Row<'a> {
    buf: &'a mut Buffer,
    hits: &'a mut Vec<(usize, Range<u16>)>,
    /// Columns a widget painted, indexed from `x0`. The effect skips these.
    taken: &'a mut [bool],
    x0: u16,
    y: u16,
    end: u16,
}

impl Row<'_> {
    fn put(&mut self, at: u16, spans: &[Span]) {
        let mut x = at;
        for s in spans {
            if x >= self.end {
                break;
            }
            let avail = (self.end - x) as usize;
            // set_stringn reports where it stopped painting. The hitbox comes
            // from that, never from a computed width, so it can never cover a
            // cell ratatui declined to paint.
            let (nx, _) = self.buf.set_stringn(x, self.y, &s.text, avail, s.style);
            for c in x..nx {
                self.taken[(c - self.x0) as usize] = true;
            }
            if let Some(i) = s.tab {
                if nx > x {
                    self.hits.push((i, x..nx));
                }
            }
            x = nx;
        }
    }
}

// ---------------------------------------------------------------- effects

/// Paint `cfg.effect` into the background of the cells `taken` leaves free.
/// `t` is a whole-second bucket: the starfield drifts once a second instead of
/// flickering every frame.
fn paint_effect(buf: &mut Buffer, rect: Rect, cfg: &StatusBar, taken: &[bool], t: u64) {
    let (from, to) = match cfg.effect {
        BarEffect::Flat => return,
        BarEffect::Starfield => {
            for y in rect.y..rect.bottom() {
                for x in rect.x..rect.right() {
                    if taken[(x - rect.x) as usize] {
                        continue;
                    }
                    let h = hash3(x as u64, y as u64, t);
                    // Sparse: ~1 cell in 64 is a bright star, 3 more are dim.
                    let (sym, fg) = match h % 64 {
                        0 => ("✦", cfg.fg.0),
                        1..=3 => ("·", Color::DarkGray),
                        _ => continue,
                    };
                    put_cell(buf, x, y, sym, Style::default().fg(fg).bg(cfg.bg.0));
                }
            }
            return;
        }
        // Indexed and Reset carry no components to blend, so stay flat rather
        // than guess what the terminal's palette resolves them to.
        BarEffect::Gradient => match (rgb_of(cfg.bg.0), rgb_of(cfg.accent.0)) {
            (Some(a), Some(b)) => (a, b),
            _ => return,
        },
    };
    let span = rect.w.saturating_sub(1).max(1) as i32;
    for x in rect.x..rect.right() {
        if taken[(x - rect.x) as usize] {
            continue;
        }
        let f = (x - rect.x) as i32;
        let bg = Color::Rgb(
            lerp(from.0, to.0, f, span),
            lerp(from.1, to.1, f, span),
            lerp(from.2, to.2, f, span),
        );
        for y in rect.y..rect.bottom() {
            put_cell(buf, x, y, " ", Style::default().fg(cfg.fg.0).bg(bg));
        }
    }
}

fn rgb_of(c: Color) -> Option<(u8, u8, u8)> {
    match c {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

fn lerp(a: u8, b: u8, num: i32, den: i32) -> u8 {
    (a as i32 + (b as i32 - a as i32) * num / den) as u8
}

/// Deterministic in (x, y, t) — no RNG state to carry between frames.
fn hash3(x: u64, y: u64, t: u64) -> u64 {
    let mut h = x.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ y.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ t.wrapping_mul(0x1656_67B1_9E37_79F9);
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h
}

// ---------------------------------------------------------------- widgets

fn widget(name: &str, cfg: &StatusBar, ctx: &Ctx, base: Style) -> Vec<Span> {
    let accent = base.fg(cfg.accent.0);
    match name {
        "session" => vec![Span::new(
            format!(" {} ", ctx.session),
            accent.add_modifier(Modifier::BOLD),
        )],
        "mode" => {
            let glyph = if ctx.mode == "free" { "⊡" } else { "⊞" };
            vec![Span::new(format!("{glyph} {}", ctx.mode), base)]
        }
        "tabs" => ctx
            .tabs
            .iter()
            .enumerate()
            .map(|(i, (name, active))| {
                let style = if *active {
                    base.bg(cfg.accent.0)
                        .fg(cfg.bg.0)
                        .add_modifier(Modifier::BOLD)
                } else {
                    base
                };
                Span {
                    text: format!(" {name} "),
                    style,
                    tab: Some(i),
                    expand: false,
                }
            })
            .collect(),
        "panes" => vec![Span::new(format!("{} panes", ctx.panes.len()), base)],
        "agents" => ctx
            .panes
            .iter()
            .filter(|(_, s)| *s != AgentState::Idle)
            .map(|(_, s)| {
                let style = match s {
                    AgentState::Attention => base.fg(WARN),
                    AgentState::Done => base.fg(OK),
                    _ => base,
                };
                Span::new(s.glyph(), style)
            })
            .collect(),
        "alerts" if ctx.alerts > 0 => {
            vec![Span::new(format!("● {}", ctx.alerts), base.fg(WARN))]
        }
        "zoom" if ctx.zoomed => vec![Span::new("⛶", base)],
        "prefix" if ctx.pending_prefix => vec![Span::new(
            "PREFIX",
            base.bg(cfg.accent.0)
                .fg(cfg.bg.0)
                .add_modifier(Modifier::BOLD),
        )],
        "message" => match ctx.message {
            Some(m) if !m.is_empty() => vec![Span::new(m, base.fg(cfg.accent.0))],
            _ => Vec::new(),
        },
        "time" => vec![Span::new(now(&cfg.time_format), base)],
        "date" => vec![Span::new(now("%Y-%m-%d"), base)],
        "host" => vec![Span::new(hostname(), base)],
        "load" => match loadavg() {
            Some(l) => vec![Span::new(format!("{l:.1}"), base)],
            None => Vec::new(),
        },
        "battery" => match battery() {
            Some(p) => vec![Span::new(format!("{p}%"), base)],
            None => Vec::new(),
        },
        "spacer" => vec![Span {
            text: String::new(),
            style: base,
            tab: None,
            expand: true,
        }],
        "alerts" | "zoom" | "prefix" => Vec::new(),
        other => vec![Span::new(format!("?{other}"), base.fg(WARN))],
    }
}

// ------------------------------------------------------------------ time

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now(fmt: &str) -> String {
    let secs = now_secs() as i64;
    format_time(fmt, secs, local_offset(secs))
}

/// UTC offset in seconds for `secs`, via `localtime_r`. Falls back to UTC.
fn local_offset(secs: i64) -> i32 {
    unsafe {
        let t = secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            0
        } else {
            tm.tm_gmtoff as i32
        }
    }
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `strftime`-ish formatting. Supports `%H %M %S %d %m %Y %y %a %b %p %I` and
/// `%%`; anything else is passed through unchanged.
pub(crate) fn format_time(fmt: &str, secs_since_epoch: i64, utc_offset_secs: i32) -> String {
    let t = secs_since_epoch + utc_offset_secs as i64;
    let days = t.div_euclid(86_400);
    let sod = t.rem_euclid(86_400);
    let (hh, mm, ss) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    let (y, mo, d) = civil_from_days(days);
    let wd = (days + 4).rem_euclid(7) as usize;

    let mut out = String::with_capacity(fmt.len() + 8);
    let mut it = fmt.chars();
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match it.next() {
            None => out.push('%'),
            Some('H') => out.push_str(&format!("{hh:02}")),
            Some('M') => out.push_str(&format!("{mm:02}")),
            Some('S') => out.push_str(&format!("{ss:02}")),
            Some('d') => out.push_str(&format!("{d:02}")),
            Some('m') => out.push_str(&format!("{mo:02}")),
            Some('Y') => out.push_str(&format!("{y:04}")),
            Some('y') => out.push_str(&format!("{:02}", y.rem_euclid(100))),
            Some('a') => out.push_str(DAYS[wd]),
            Some('b') => out.push_str(MONTHS[(mo - 1) as usize]),
            Some('p') => out.push_str(if hh < 12 { "AM" } else { "PM" }),
            Some('I') => {
                let h12 = match hh % 12 {
                    0 => 12,
                    h => h,
                };
                out.push_str(&format!("{h12:02}"));
            }
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out
}

/// Days since the epoch -> (year, month 1-12, day 1-31). Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------- system

/// Short hostname (everything before the first dot), like `uname -n | cut -d. -f1`.
///
/// Cached: it cannot change during a session, and the bar redraws every frame.
fn hostname() -> &'static str {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(read_hostname)
}

fn read_hostname() -> String {
    let mut buf = [0i8; 256];
    let full = unsafe {
        if libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) == 0 {
            let bytes: Vec<u8> = buf
                .iter()
                .take_while(|b| **b != 0)
                .map(|b| *b as u8)
                .collect();
            String::from_utf8_lossy(&bytes).into_owned()
        } else {
            String::new()
        }
    };
    let full = if full.is_empty() {
        std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_else(|| "localhost".into())
    } else {
        full
    };
    full.split('.').next().unwrap_or("localhost").to_string()
}

/// 1-minute load average, or `None` if the OS won't say.
fn loadavg() -> Option<f64> {
    let mut avg = [0f64; 3];
    let n = unsafe { libc::getloadavg(avg.as_mut_ptr(), 1) };
    (n >= 1).then_some(avg[0])
}

/// Battery percentage, Linux only — everywhere else the cheap read doesn't exist.
fn battery() -> Option<u8> {
    #[cfg(target_os = "linux")]
    {
        let dir = std::fs::read_dir("/sys/class/power_supply").ok()?;
        for e in dir.flatten() {
            if !e.file_name().to_string_lossy().starts_with("BAT") {
                continue;
            }
            if let Ok(s) = std::fs::read_to_string(e.path().join("capacity")) {
                if let Ok(p) = s.trim().parse() {
                    return Some(p);
                }
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect as RRect;

    fn bar_with(left: &[&str], center: &[&str], right: &[&str]) -> Bar {
        Bar {
            enabled: true,
            left: left.iter().map(|s| s.to_string()).collect(),
            center: center.iter().map(|s| s.to_string()).collect(),
            right: right.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Stock colours, separator and `BarEffect::Flat` — several tests below
    /// diff an effect against what this draws.
    fn cfg() -> StatusBar {
        StatusBar::default()
    }

    fn make_ctx<'a>(tabs: &'a [(String, bool)], panes: &'a [(String, AgentState)]) -> Ctx<'a> {
        Ctx {
            session: "main",
            mode: "tiling",
            zoomed: false,
            tabs,
            panes,
            alerts: 0,
            message: None,
            pending_prefix: false,
        }
    }

    fn tabs(names: &[&str], active: usize) -> Vec<(String, bool)> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.to_string(), i == active))
            .collect()
    }

    fn row(buf: &Buffer, w: u16) -> String {
        (0..w).map(|x| buf.cell((x, 0)).unwrap().symbol()).collect()
    }

    fn buffer(w: u16) -> Buffer {
        Buffer::empty(RRect::new(0, 0, w, 1))
    }

    // ---- format_time

    #[test]
    fn format_time_renders_the_epoch() {
        assert_eq!(
            format_time("%Y-%m-%d %H:%M:%S", 0, 0),
            "1970-01-01 00:00:00"
        );
    }

    #[test]
    fn format_time_matches_a_known_timestamp() {
        assert_eq!(
            format_time("%Y-%m-%d %H:%M:%S", 1_700_000_000, 0),
            "2023-11-14 22:13:20"
        );
    }

    #[test]
    fn format_time_counts_the_leap_day() {
        // 2024-02-29T12:00:00Z
        assert_eq!(format_time("%Y-%m-%d", 1_709_208_000, 0), "2024-02-29");
    }

    #[test]
    fn weekday_month_and_meridiem_follow_the_date() {
        // 2023-11-14 22:13:20 UTC is a Tuesday.
        assert_eq!(
            format_time("%a %b %p %I", 1_700_000_000, 0),
            "Tue Nov PM 10"
        );
        assert_eq!(format_time("%a %b %p %I", 0, 0), "Thu Jan AM 12");
    }

    #[test]
    fn percent_escapes_and_unknown_directives_pass_through() {
        assert_eq!(format_time("100%%", 0, 0), "100%");
        assert_eq!(format_time("%q", 0, 0), "%q");
        assert_eq!(format_time("%y", 1_700_000_000, 0), "23");
    }

    #[test]
    fn a_positive_utc_offset_moves_the_clock_forward() {
        // +02:00
        assert_eq!(format_time("%H:%M", 0, 7200), "02:00");
        assert_eq!(
            format_time("%Y-%m-%d %H", 1_700_000_000, 7200),
            "2023-11-15 00"
        );
    }

    #[test]
    fn a_negative_utc_offset_can_cross_back_a_day() {
        // -05:00 crosses back over midnight.
        assert_eq!(
            format_time("%Y-%m-%d %H:%M", 0, -18_000),
            "1969-12-31 19:00"
        );
    }

    // ---- draw

    #[test]
    fn default_config_fills_the_row() {
        let bar = cfg().footer.clone();
        let t = tabs(&["one", "two"], 0);
        let p = vec![("sh".into(), AgentState::Busy)];
        let mut buf = buffer(40);
        draw(
            &mut buf,
            Rect::new(0, 0, 40, 1),
            &cfg(),
            &bar,
            &make_ctx(&t, &p),
        );
        assert_eq!(row(&buf, 40).chars().count(), 40);
    }

    #[test]
    fn groups_hug_their_edges() {
        let bar = bar_with(&["session"], &[], &["panes"]);
        let p = vec![
            ("a".into(), AgentState::Idle),
            ("b".into(), AgentState::Idle),
        ];
        let mut buf = buffer(40);
        draw(
            &mut buf,
            Rect::new(0, 0, 40, 1),
            &cfg(),
            &bar,
            &make_ctx(&[], &p),
        );
        let line = row(&buf, 40);
        assert!(line.starts_with(" main "), "{line:?}");
        assert!(line.ends_with("2 panes"), "{line:?}");
    }

    #[test]
    fn tab_hitboxes_do_not_overlap_and_map_a_click_to_its_tab() {
        let bar = bar_with(&["tabs"], &[], &[]);
        let t = tabs(&["one", "two", "three"], 1);
        let mut buf = buffer(40);
        let hits = draw(
            &mut buf,
            Rect::new(0, 0, 40, 1),
            &cfg(),
            &bar,
            &make_ctx(&t, &[]),
        );
        assert_eq!(hits.len(), 3);
        assert_eq!(
            hits.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        for pair in hits.windows(2) {
            assert!(pair[0].1.end <= pair[1].1.start, "overlap: {hits:?}");
        }
        // " one " is 5 columns, so tab 1 starts at column 5.
        assert_eq!(hits[1].1, 5..10);
        let click = 6;
        let idx = hits
            .iter()
            .find(|(_, r)| r.contains(&click))
            .map(|(i, _)| *i);
        assert_eq!(idx, Some(1));
    }

    /// What `put` charges for a span must be what ratatui paints, or every
    /// hitbox after it is off by the difference.
    #[test]
    fn span_width_matches_what_ratatui_draws() {
        for s in [" 1:a\tb ", "ｶﾞ", "あ", "ab", "aｶﾞb", "\u{1}"] {
            let mut buf = buffer(20);
            let (x, _) = buf.set_stringn(0, 0, s, 20, Style::default());
            assert_eq!(sw(s), x as usize, "{s:?}");
        }
    }

    #[test]
    fn hitboxes_cover_exactly_the_painted_label() {
        let bar = bar_with(&["tabs"], &[], &[]);
        let t = tabs(&["a\tb", "ｶﾞ", "x"], 0);
        let mut buf = buffer(40);
        let hits = draw(
            &mut buf,
            Rect::new(0, 0, 40, 1),
            &cfg(),
            &bar,
            &make_ctx(&t, &[]),
        );
        let painted: Vec<String> = hits
            .iter()
            .map(|(_, r)| {
                (r.start..r.end)
                    .map(|x| buf.cell((x, 0)).unwrap().symbol())
                    .collect()
            })
            .collect();
        // The tab char is dropped; "ｶﾞ" is one 2-column grapheme, so ratatui
        // parks it in one cell and blanks the next — hence the extra space.
        assert_eq!(painted, vec![" ab ", " ｶﾞ  ", " x "]);
    }

    #[test]
    fn active_tab_scrolls_into_view() {
        let bar = bar_with(&["tabs"], &[], &[]);
        let names: Vec<String> = (0..20).map(|i| format!("tab{i}")).collect();
        let t = tabs(&names.iter().map(|s| s.as_str()).collect::<Vec<_>>(), 15);
        let mut buf = buffer(80);
        let hits = draw(
            &mut buf,
            Rect::new(0, 0, 80, 1),
            &cfg(),
            &bar,
            &make_ctx(&t, &[]),
        );
        let (_, r) = hits
            .iter()
            .find(|(i, _)| *i == 15)
            .expect("active tab has no hitbox");
        let painted: String = (r.start..r.end)
            .map(|x| buf.cell((x, 0)).unwrap().symbol())
            .collect();
        assert_eq!(painted, " tab15 ");
    }

    #[test]
    fn narrow_bar_drops_the_centre() {
        let bar = cfg().footer.clone();
        let t = tabs(&["alpha", "beta"], 0);
        let mut buf = buffer(10);
        draw(
            &mut buf,
            Rect::new(0, 0, 10, 1),
            &cfg(),
            &bar,
            &make_ctx(&t, &[]),
        );
        let line = row(&buf, 10);
        assert!(!line.contains("alpha"), "{line:?}");
        assert_eq!(line.chars().count(), 10);
    }

    #[test]
    fn one_column_bar_does_not_panic() {
        let bar = cfg().footer.clone();
        let t = tabs(&["alpha"], 0);
        let p = vec![("sh".into(), AgentState::Attention)];
        let mut buf = buffer(1);
        draw(
            &mut buf,
            Rect::new(0, 0, 1, 1),
            &cfg(),
            &bar,
            &make_ctx(&t, &p),
        );
    }

    #[test]
    fn the_agents_widget_shows_only_non_idle_panes() {
        let bar = bar_with(&["agents"], &[], &[]);
        let idle = vec![
            ("a".into(), AgentState::Idle),
            ("b".into(), AgentState::Idle),
        ];
        let mut buf = buffer(20);
        draw(
            &mut buf,
            Rect::new(0, 0, 20, 1),
            &cfg(),
            &bar,
            &make_ctx(&[], &idle),
        );
        assert_eq!(row(&buf, 20).trim(), "");

        let busy = vec![
            ("a".into(), AgentState::Busy),
            ("b".into(), AgentState::Idle),
            ("c".into(), AgentState::Done),
        ];
        let mut buf = buffer(20);
        draw(
            &mut buf,
            Rect::new(0, 0, 20, 1),
            &cfg(),
            &bar,
            &make_ctx(&[], &busy),
        );
        assert_eq!(row(&buf, 20).trim(), "◐✓");
    }

    #[test]
    fn conditional_widgets_are_silent() {
        let bar = bar_with(&["alerts", "zoom", "prefix"], &[], &[]);
        let mut buf = buffer(20);
        draw(
            &mut buf,
            Rect::new(0, 0, 20, 1),
            &cfg(),
            &bar,
            &make_ctx(&[], &[]),
        );
        assert_eq!(row(&buf, 20).trim(), "");

        let mut c = make_ctx(&[], &[]);
        c.alerts = 3;
        c.zoomed = true;
        c.pending_prefix = true;
        let mut buf = buffer(30);
        draw(&mut buf, Rect::new(0, 0, 30, 1), &cfg(), &bar, &c);
        let line = row(&buf, 30);
        assert!(
            line.contains("● 3") && line.contains('⛶') && line.contains("PREFIX"),
            "{line:?}"
        );
    }

    /// A name in `WIDGETS` with no arm in `widget()` would be advertised as
    /// supported and then render as `?name`.
    #[test]
    fn every_advertised_widget_has_an_implementation() {
        let t = tabs(&["one"], 0);
        let p = vec![("sh".into(), AgentState::Busy)];
        let mut c = make_ctx(&t, &p);
        c.alerts = 1;
        c.zoomed = true;
        c.pending_prefix = true;
        c.message = Some("hi");
        for name in WIDGETS {
            let spans = widget(name, &cfg(), &c, Style::default());
            assert!(
                !spans.iter().any(|s| s.text.starts_with('?')),
                "{name} has no arm in widget()"
            );
        }
    }

    #[test]
    fn unknown_widget_is_visible() {
        let bar = bar_with(&["bogus"], &[], &[]);
        let mut buf = buffer(20);
        let hits = draw(
            &mut buf,
            Rect::new(0, 0, 20, 1),
            &cfg(),
            &bar,
            &make_ctx(&[], &[]),
        );
        assert_eq!(row(&buf, 20).trim(), "?bogus");
        assert!(hits.is_empty());
    }

    #[test]
    fn spacer_pushes_widgets_apart() {
        let bar = bar_with(&["session", "spacer", "panes"], &[], &[]);
        let mut buf = buffer(40);
        draw(
            &mut buf,
            Rect::new(0, 0, 40, 1),
            &cfg(),
            &bar,
            &make_ctx(&[], &[]),
        );
        let line = row(&buf, 40);
        assert!(line.starts_with(" main "), "{line:?}");
        assert!(line.ends_with("0 panes"), "{line:?}");
    }

    #[test]
    fn draw_respects_a_rect_offset() {
        let bar = bar_with(&["tabs"], &[], &[]);
        let t = tabs(&["one"], 0);
        let mut buf = Buffer::empty(RRect::new(0, 0, 20, 2));
        let hits = draw(
            &mut buf,
            Rect::new(5, 1, 10, 1),
            &cfg(),
            &bar,
            &make_ctx(&t, &[]),
        );
        assert_eq!(hits, vec![(0, 5..10)]);
        // Nothing was written outside the rect.
        assert_eq!(buf.cell((4, 1)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), " ");
    }

    // ---- rows

    #[test]
    fn header_and_footer_render_their_own_widgets() {
        let c = cfg();
        let header = bar_with(&["panes"], &[], &[]);
        let footer = bar_with(&["session"], &[], &[]);
        let mut buf = Buffer::empty(RRect::new(0, 0, 20, 2));
        draw(
            &mut buf,
            Rect::new(0, 0, 20, 1),
            &c,
            &header,
            &make_ctx(&[], &[]),
        );
        let mut foot = buffer(20);
        draw(
            &mut foot,
            Rect::new(0, 0, 20, 1),
            &c,
            &footer,
            &make_ctx(&[], &[]),
        );
        let top: String = (0..20)
            .map(|x| buf.cell((x, 0)).unwrap().symbol())
            .collect();
        assert_eq!(top.trim(), "0 panes");
        assert_eq!(row(&foot, 20).trim(), "main");
    }

    // ---- effects

    fn with_effect(e: BarEffect) -> StatusBar {
        StatusBar { effect: e, ..cfg() }
    }

    fn draw_bar(c: &StatusBar, w: u16) -> (Buffer, Vec<(usize, Range<u16>)>) {
        let bar = bar_with(&["session"], &["tabs"], &["panes"]);
        let t = tabs(&["one", "two"], 1);
        let p = vec![("sh".into(), AgentState::Idle)];
        let mut buf = buffer(w);
        let hits = draw(&mut buf, Rect::new(0, 0, w, 1), c, &bar, &make_ctx(&t, &p));
        (buf, hits)
    }

    #[test]
    fn the_flat_effect_leaves_the_row_exactly_as_drawn() {
        let (buf, _) = draw_bar(&cfg(), 40);
        assert_eq!(row(&buf, 40), " main          one  two          1 panes");
    }

    #[test]
    fn effects_leave_widgets_and_hitboxes_alone() {
        for w in [1, 2, 7, 40] {
            let (flat, flat_hits) = draw_bar(&cfg(), w);
            for e in [BarEffect::Starfield, BarEffect::Gradient] {
                let (buf, hits) = draw_bar(&with_effect(e), w);
                assert_eq!(hits, flat_hits, "{e:?} at width {w}");
                let cells = hits.iter().flat_map(|(_, r)| r.clone());
                let glyphs = (0..w).filter(|x| flat.cell((*x, 0)).unwrap().symbol() != " ");
                for x in cells.chain(glyphs) {
                    assert_eq!(buf.cell((x, 0)), flat.cell((x, 0)), "{e:?} at {x}/{w}");
                }
            }
        }
    }

    #[test]
    fn starfield_holds_still_within_a_second_and_drifts_after() {
        let c = with_effect(BarEffect::Starfield);
        let bar = bar_with(&[], &[], &[]);
        let paint = |t: u64| {
            let mut buf = buffer(80);
            draw(
                &mut buf,
                Rect::new(0, 0, 80, 1),
                &c,
                &bar,
                &make_ctx(&[], &[]),
            );
            paint_effect(&mut buf, Rect::new(0, 0, 80, 1), &c, &[false; 80], t);
            buf
        };
        assert_eq!(paint(1_700_000_000), paint(1_700_000_000));
        assert_ne!(paint(1_700_000_000), paint(1_700_000_001));

        // And the real clock path: two frames inside one second agree.
        for _ in 0..3 {
            let t0 = now_secs();
            let (a, _) = draw_bar(&c, 80);
            let (b, _) = draw_bar(&c, 80);
            if now_secs() == t0 {
                assert_eq!(a, b);
                break;
            }
        }
    }

    #[test]
    fn starfield_actually_paints_something() {
        let c = with_effect(BarEffect::Starfield);
        let mut buf = buffer(80);
        paint_effect(&mut buf, Rect::new(0, 0, 80, 1), &c, &[false; 80], 7);
        assert!(row(&buf, 80).trim() != "");
    }

    #[test]
    fn gradient_blends_bg_towards_accent() {
        let c = with_effect(BarEffect::Gradient);
        let bar = bar_with(&[], &[], &[]);
        let mut buf = buffer(40);
        draw(
            &mut buf,
            Rect::new(0, 0, 40, 1),
            &c,
            &bar,
            &make_ctx(&[], &[]),
        );
        assert_eq!(buf.cell((0, 0)).unwrap().bg, c.bg.0);
        assert_eq!(buf.cell((39, 0)).unwrap().bg, c.accent.0);
        assert_ne!(buf.cell((20, 0)).unwrap().bg, c.bg.0);
    }

    #[test]
    fn gradient_without_rgb_stays_flat() {
        let bar = bar_with(&["session"], &[], &[]);
        for bg in [Color::Indexed(4), Color::Reset] {
            let c = StatusBar {
                effect: BarEffect::Gradient,
                bg: crate::config::Rgb(bg),
                ..cfg()
            };
            let mut buf = buffer(20);
            draw(
                &mut buf,
                Rect::new(0, 0, 20, 1),
                &c,
                &bar,
                &make_ctx(&[], &[]),
            );
            for x in 0..20 {
                assert_eq!(buf.cell((x, 0)).unwrap().bg, bg, "{bg:?} at {x}");
            }
        }
    }

    #[test]
    fn degenerate_widths_do_not_panic() {
        for e in [BarEffect::Flat, BarEffect::Starfield, BarEffect::Gradient] {
            let c = with_effect(e);
            let bar = bar_with(&["session"], &["tabs"], &["time"]);
            let t = tabs(&["alpha"], 0);
            for w in [0, 1] {
                let mut buf = buffer(w.max(1));
                draw(
                    &mut buf,
                    Rect::new(0, 0, w, 1),
                    &c,
                    &bar,
                    &make_ctx(&t, &[]),
                );
            }
        }
    }
}
