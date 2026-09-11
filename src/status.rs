//! The one-row status bar.
//!
//! The bar is composed from widget names listed in the config
//! ([`StatusBar::left`] / `center` / `right`), so users can build their own.
//! [`draw`] returns the on-screen column range of each tab it rendered, so the
//! app can turn a click into a tab index.

use std::ops::Range;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::agent::AgentState;
use crate::config::StatusBar;
use crate::layout::Rect;

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

/// Every supported widget name, for the settings UI and for validation.
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
        Span { text: text.into(), style, tab: None, expand: false }
    }
}

fn sw(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| sw(&s.text)).sum()
}

/// Render the bar into `rect`; returns `(tab index, columns)` for each tab drawn.
pub fn draw(buf: &mut Buffer, rect: Rect, cfg: &StatusBar, ctx: &Ctx) -> Vec<(usize, Range<u16>)> {
    let base = Style::default().fg(cfg.fg.0).bg(cfg.bg.0);
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_symbol(" ").set_style(base);
            }
        }
    }
    if rect.w == 0 || rect.h == 0 {
        return Vec::new();
    }

    let w = rect.w as usize;
    let sep = || Span::new(cfg.separator.clone(), base);
    let mut left = join(chunks(&cfg.left, cfg, ctx, base), &sep);
    let mut center_chunks = chunks(&cfg.center, cfg, ctx, base);
    let right = join(chunks(&cfg.right, cfg, ctx, base), &sep);

    // Too wide? Drop centre widgets first, then truncate the left group.
    let rw = spans_width(&right);
    let mut center = join(center_chunks.clone(), &sep);
    while spans_width(&left) + spans_width(&center) + rw > w && !center_chunks.is_empty() {
        center_chunks.pop();
        center = join(center_chunks.clone(), &sep);
    }
    let lw_budget = w.saturating_sub(rw);
    if spans_width(&left) > lw_budget {
        left = truncate(left, lw_budget);
    }

    // Spacers soak up whatever is left over.
    let lw = spans_width(&left);
    let cw = spans_width(&center);
    let free = w.saturating_sub(lw + cw + rw);
    expand_spacers(&mut left, free);

    let lw = spans_width(&left);
    let gap = w.saturating_sub(lw + rw);
    let cw = spans_width(&center).min(gap);

    let mut hits = Vec::new();
    let end = rect.right();
    put(buf, &mut hits, rect.x, rect.y, end, &left);
    put(
        buf,
        &mut hits,
        rect.x + (lw + (gap - cw) / 2) as u16,
        rect.y,
        end,
        &center,
    );
    put(buf, &mut hits, end - rw.min(w) as u16, rect.y, end, &right);
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

fn put(
    buf: &mut Buffer,
    hits: &mut Vec<(usize, Range<u16>)>,
    x0: u16,
    y: u16,
    end: u16,
    spans: &[Span],
) {
    let mut x = x0;
    for s in spans {
        if x >= end {
            break;
        }
        let avail = (end - x) as usize;
        let wid = sw(&s.text).min(avail);
        buf.set_stringn(x, y, &s.text, avail, s.style);
        if let Some(i) = s.tab {
            if wid > 0 {
                hits.push((i, x..x + wid as u16));
            }
        }
        x += wid as u16;
    }
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
                    base.bg(cfg.accent.0).fg(cfg.bg.0).add_modifier(Modifier::BOLD)
                } else {
                    base
                };
                Span { text: format!(" {name} "), style, tab: Some(i), expand: false }
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
            base.bg(cfg.accent.0).fg(cfg.bg.0).add_modifier(Modifier::BOLD),
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
        "spacer" => vec![Span { text: String::new(), style: base, tab: None, expand: true }],
        "alerts" | "zoom" | "prefix" => Vec::new(),
        other => vec![Span::new(format!("?{other}"), base.fg(WARN))],
    }
}

// ------------------------------------------------------------------ time

fn now(fmt: &str) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
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

/// Short hostname (everything before the first dot).
fn hostname() -> String {
    let mut buf = [0i8; 256];
    let full = unsafe {
        if libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) == 0 {
            let bytes: Vec<u8> = buf.iter().take_while(|b| **b != 0).map(|b| *b as u8).collect();
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

    fn cfg_with(left: &[&str], center: &[&str], right: &[&str]) -> StatusBar {
        StatusBar {
            left: left.iter().map(|s| s.to_string()).collect(),
            center: center.iter().map(|s| s.to_string()).collect(),
            right: right.iter().map(|s| s.to_string()).collect(),
            ..StatusBar::default()
        }
    }

    fn ctx<'a>(tabs: &'a [(String, bool)], panes: &'a [(String, AgentState)]) -> Ctx<'a> {
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
    fn epoch_zero() {
        assert_eq!(format_time("%Y-%m-%d %H:%M:%S", 0, 0), "1970-01-01 00:00:00");
    }

    #[test]
    fn known_epoch() {
        assert_eq!(
            format_time("%Y-%m-%d %H:%M:%S", 1_700_000_000, 0),
            "2023-11-14 22:13:20"
        );
    }

    #[test]
    fn leap_day() {
        // 2024-02-29T12:00:00Z
        assert_eq!(format_time("%Y-%m-%d", 1_709_208_000, 0), "2024-02-29");
    }

    #[test]
    fn weekday_month_meridiem() {
        // 2023-11-14 22:13:20 UTC is a Tuesday.
        assert_eq!(format_time("%a %b %p %I", 1_700_000_000, 0), "Tue Nov PM 10");
        assert_eq!(format_time("%a %b %p %I", 0, 0), "Thu Jan AM 12");
    }

    #[test]
    fn percent_and_unknown() {
        assert_eq!(format_time("100%%", 0, 0), "100%");
        assert_eq!(format_time("%q", 0, 0), "%q");
        assert_eq!(format_time("%y", 1_700_000_000, 0), "23");
    }

    #[test]
    fn positive_offset() {
        // +02:00
        assert_eq!(format_time("%H:%M", 0, 7200), "02:00");
        assert_eq!(format_time("%Y-%m-%d %H", 1_700_000_000, 7200), "2023-11-15 00");
    }

    #[test]
    fn negative_offset() {
        // -05:00 crosses back over midnight.
        assert_eq!(format_time("%Y-%m-%d %H:%M", 0, -18_000), "1969-12-31 19:00");
    }

    // ---- draw

    #[test]
    fn default_config_fills_the_row() {
        let cfg = StatusBar::default();
        let t = tabs(&["one", "two"], 0);
        let p = vec![("sh".into(), AgentState::Busy)];
        let mut buf = buffer(40);
        draw(&mut buf, Rect::new(0, 0, 40, 1), &cfg, &ctx(&t, &p));
        assert_eq!(row(&buf, 40).chars().count(), 40);
    }

    #[test]
    fn groups_hug_their_edges() {
        let cfg = cfg_with(&["session"], &[], &["panes"]);
        let p = vec![("a".into(), AgentState::Idle), ("b".into(), AgentState::Idle)];
        let mut buf = buffer(40);
        draw(&mut buf, Rect::new(0, 0, 40, 1), &cfg, &ctx(&[], &p));
        let line = row(&buf, 40);
        assert!(line.starts_with(" main "), "{line:?}");
        assert!(line.ends_with("2 panes"), "{line:?}");
    }

    #[test]
    fn tab_hitboxes() {
        let cfg = cfg_with(&["tabs"], &[], &[]);
        let t = tabs(&["one", "two", "three"], 1);
        let mut buf = buffer(40);
        let hits = draw(&mut buf, Rect::new(0, 0, 40, 1), &cfg, &ctx(&t, &[]));
        assert_eq!(hits.len(), 3);
        assert_eq!(hits.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0, 1, 2]);
        for pair in hits.windows(2) {
            assert!(pair[0].1.end <= pair[1].1.start, "overlap: {hits:?}");
        }
        // " one " is 5 columns, so tab 1 starts at column 5.
        assert_eq!(hits[1].1, 5..10);
        let click = 6;
        let idx = hits.iter().find(|(_, r)| r.contains(&click)).map(|(i, _)| *i);
        assert_eq!(idx, Some(1));
    }

    #[test]
    fn narrow_bar_drops_the_centre() {
        let cfg = StatusBar::default();
        let t = tabs(&["alpha", "beta"], 0);
        let mut buf = buffer(10);
        draw(&mut buf, Rect::new(0, 0, 10, 1), &cfg, &ctx(&t, &[]));
        let line = row(&buf, 10);
        assert!(!line.contains("alpha"), "{line:?}");
        assert_eq!(line.chars().count(), 10);
    }

    #[test]
    fn one_column_bar_does_not_panic() {
        let cfg = StatusBar::default();
        let t = tabs(&["alpha"], 0);
        let p = vec![("sh".into(), AgentState::Attention)];
        let mut buf = buffer(1);
        draw(&mut buf, Rect::new(0, 0, 1, 1), &cfg, &ctx(&t, &p));
    }

    #[test]
    fn agents_widget() {
        let cfg = cfg_with(&["agents"], &[], &[]);
        let idle = vec![("a".into(), AgentState::Idle), ("b".into(), AgentState::Idle)];
        let mut buf = buffer(20);
        draw(&mut buf, Rect::new(0, 0, 20, 1), &cfg, &ctx(&[], &idle));
        assert_eq!(row(&buf, 20).trim(), "");

        let busy = vec![
            ("a".into(), AgentState::Busy),
            ("b".into(), AgentState::Idle),
            ("c".into(), AgentState::Done),
        ];
        let mut buf = buffer(20);
        draw(&mut buf, Rect::new(0, 0, 20, 1), &cfg, &ctx(&[], &busy));
        assert_eq!(row(&buf, 20).trim(), "◐✓");
    }

    #[test]
    fn conditional_widgets_are_silent() {
        let cfg = cfg_with(&["alerts", "zoom", "prefix"], &[], &[]);
        let mut buf = buffer(20);
        draw(&mut buf, Rect::new(0, 0, 20, 1), &cfg, &ctx(&[], &[]));
        assert_eq!(row(&buf, 20).trim(), "");

        let mut c = ctx(&[], &[]);
        c.alerts = 3;
        c.zoomed = true;
        c.pending_prefix = true;
        let mut buf = buffer(30);
        draw(&mut buf, Rect::new(0, 0, 30, 1), &cfg, &c);
        let line = row(&buf, 30);
        assert!(line.contains("● 3") && line.contains('⛶') && line.contains("PREFIX"), "{line:?}");
    }

    #[test]
    fn unknown_widget_is_visible() {
        let cfg = cfg_with(&["bogus"], &[], &[]);
        let mut buf = buffer(20);
        let hits = draw(&mut buf, Rect::new(0, 0, 20, 1), &cfg, &ctx(&[], &[]));
        assert_eq!(row(&buf, 20).trim(), "?bogus");
        assert!(hits.is_empty());
    }

    #[test]
    fn spacer_pushes_widgets_apart() {
        let cfg = cfg_with(&["session", "spacer", "panes"], &[], &[]);
        let mut buf = buffer(40);
        draw(&mut buf, Rect::new(0, 0, 40, 1), &cfg, &ctx(&[], &[]));
        let line = row(&buf, 40);
        assert!(line.starts_with(" main "), "{line:?}");
        assert!(line.ends_with("0 panes"), "{line:?}");
    }

    #[test]
    fn draw_respects_a_rect_offset() {
        let cfg = cfg_with(&["tabs"], &[], &[]);
        let t = tabs(&["one"], 0);
        let mut buf = Buffer::empty(RRect::new(0, 0, 20, 2));
        let hits = draw(&mut buf, Rect::new(5, 1, 10, 1), &cfg, &ctx(&t, &[]));
        assert_eq!(hits, vec![(0, 5..10)]);
        // Nothing was written outside the rect.
        assert_eq!(buf.cell((4, 1)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), " ");
    }
}
