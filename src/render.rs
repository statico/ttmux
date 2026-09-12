//! Drawing: pane borders and pane contents into a ratatui `Buffer`.
//!
//! Everything here clips to the buffer, so an out-of-bounds rect draws less
//! rather than panicking.

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::{Appearance, BorderStyle, TitlePosition};
use crate::layout::Rect;

/// The six glyphs a border is drawn from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub tl: char,
    pub tr: char,
    pub bl: char,
    pub br: char,
    pub h: char,
    pub v: char,
}

/// Glyphs for a border style. `None` yields spaces so callers never get junk.
/// Write a cell from scratch.
///
/// `Cell::set_style` *merges* modifiers into whatever the cell already holds
/// and leaves its underline colour alone, so painting a pane over another one
/// inherits its underline, bold or inverse. Resetting first is what makes a
/// float opaque.
fn put_cell(buf: &mut Buffer, x: u16, y: u16, symbol: &str, style: Style) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.reset();
        cell.set_symbol(symbol);
        cell.set_style(style);
    }
}

pub fn frame_chars(style: BorderStyle) -> Frame {
    let (tl, tr, bl, br, h, v) = match style {
        BorderStyle::Curved => ('╭', '╮', '╰', '╯', '─', '│'),
        BorderStyle::Square => ('┌', '┐', '└', '┘', '─', '│'),
        BorderStyle::Heavy => ('┏', '┓', '┗', '┛', '━', '┃'),
        BorderStyle::Double => ('╔', '╗', '╚', '╝', '═', '║'),
        BorderStyle::Dashed => ('╭', '╮', '╰', '╯', '┄', '┆'),
        BorderStyle::None => (' ', ' ', ' ', ' ', ' ', ' '),
    };
    Frame {
        tl,
        tr,
        bl,
        br,
        h,
        v,
    }
}

/// Marker drawn on a zoomed pane's border.
const ZOOM: &str = " ⛶ ";

fn put(buf: &mut Buffer, x: u16, y: u16, ch: char, style: Style) {
    put_cell(buf, x, y, ch.encode_utf8(&mut [0u8; 4]), style);
}

/// Write `s` at `(x, y)`, honouring wide characters. Returns the width used.
fn put_str(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style) -> u16 {
    let mut cx = x;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0) as u16;
        if w == 0 {
            continue;
        }
        put(buf, cx, y, ch, style);
        if w == 2 {
            put_cell(buf, cx.saturating_add(1), y, "", style);
        }
        cx = cx.saturating_add(w);
    }
    cx - x
}

/// Shorten `s` to at most `max` display columns, ending in `…` if cut.
fn truncate(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > max - 1 {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Draw a pane border around the *outer* `rect`, with an optional title and
/// zoom marker.
pub fn draw_border(
    buf: &mut Buffer,
    rect: Rect,
    title: &str,
    focused: bool,
    alert: bool,
    zoomed: bool,
    cfg: &Appearance,
) {
    if cfg.border_style == BorderStyle::None || rect.w < 2 || rect.h < 2 {
        return;
    }
    let f = frame_chars(cfg.border_style);
    let colour = if alert {
        cfg.border_alert
    } else if focused {
        cfg.border_focused
    } else {
        cfg.border
    };
    let mut style = Style::new().fg(colour.into());
    if focused {
        style = style.add_modifier(Modifier::BOLD);
    }

    let (x0, y0) = (rect.x, rect.y);
    let (x1, y1) = (rect.right() - 1, rect.bottom() - 1);

    for x in x0 + 1..x1 {
        put(buf, x, y0, f.h, style);
        put(buf, x, y1, f.h, style);
    }
    for y in y0 + 1..y1 {
        put(buf, x0, y, f.v, style);
        put(buf, x1, y, f.v, style);
    }
    put(buf, x0, y0, f.tl, style);
    put(buf, x1, y0, f.tr, style);
    put(buf, x0, y1, f.bl, style);
    put(buf, x1, y1, f.br, style);

    let edge = match cfg.title_position {
        TitlePosition::Top => Some(y0),
        TitlePosition::Bottom => Some(y1),
        TitlePosition::Hidden => None,
    };

    // Title sits two cells in from the left corner, as ` title `.
    let mut title_end = x0 + 2;
    if let Some(ey) = edge {
        if !title.is_empty() && rect.w > 6 {
            let text = format!(" {} ", truncate(title, rect.w as usize - 6));
            title_end += put_str(buf, x0 + 2, ey, &text, style);
        }
    }

    if zoomed {
        let ey = edge.unwrap_or(y0);
        let mw = ZOOM.width() as u16;
        let start = x1.saturating_sub(1 + mw);
        if start > x0 && start >= title_end {
            put_str(buf, start, ey, ZOOM, style);
        }
    }
}

fn colour(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(n) => Color::Indexed(n),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Copy a vt100 screen into `rect` (the pane's *inner* rect).
pub fn draw_screen(buf: &mut Buffer, rect: Rect, screen: &vt100::Screen, dim: bool) {
    for row in 0..rect.h {
        let mut col = 0u16;
        while col < rect.w {
            let Some(vc) = screen.cell(row, col) else {
                col += 1;
                continue;
            };
            let mut style = Style::new()
                .fg(colour(vc.fgcolor()))
                .bg(colour(vc.bgcolor()));
            for (on, m) in [
                (vc.bold(), Modifier::BOLD),
                (vc.italic(), Modifier::ITALIC),
                (vc.underline(), Modifier::UNDERLINED),
                (vc.inverse(), Modifier::REVERSED),
                (vc.dim() || dim, Modifier::DIM),
            ] {
                if on {
                    style = style.add_modifier(m);
                }
            }

            let text = vc.contents();
            let w = text.width().max(1) as u16;
            let (x, y) = (rect.x + col, rect.y + row);
            put_cell(buf, x, y, if text.is_empty() { " " } else { text }, style);
            if w == 2 {
                if col + 1 < rect.w {
                    put_cell(buf, x + 1, y, "", style);
                }
                col += 2;
            } else {
                col += 1;
            }
        }
    }
}

/// Blank a rect and give it a style. An overlay is drawn on top of panes that
/// are already in the buffer, and `Buffer::set_style` restyles cells without
/// replacing their symbols, so anything that does not clear first shows the
/// pane's text bleeding through its own gaps.
pub fn clear(buf: &mut Buffer, rect: Rect, style: Style) {
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            put_cell(buf, x, y, " ", style);
        }
    }
}

/// Tint the half a dragged pane would snap into. Symbols are left alone, so
/// the pane underneath stays recognisable, and the accent is a configured
/// colour, so it reads on a light theme as well as a dark one.
pub fn draw_snap_preview(buf: &mut Buffer, rect: Rect, accent: Color) {
    let style = Style::new().bg(accent).fg(Color::Black);
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_style(style);
            }
        }
    }
}

/// Darken the L-shaped band one cell right of and below `rect`.
pub fn draw_shadow(buf: &mut Buffer, rect: Rect) {
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    let style = Style::new()
        .fg(Color::Rgb(60, 60, 70))
        .bg(Color::Rgb(10, 10, 14))
        .add_modifier(Modifier::DIM);
    let shade = |buf: &mut Buffer, x: u16, y: u16| {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_style(style); // keeps whatever symbol is already there
        }
    };
    for y in rect.y + 1..=rect.bottom() {
        shade(buf, rect.right(), y);
    }
    for x in rect.x + 1..=rect.right() {
        shade(buf, x, rect.bottom());
    }
}

#[cfg(test)]
mod tests {

    /// Every cell in `rect`, with `X` under a full house of attributes: what a
    /// pane running `ls` with an underlined filename leaves in the buffer.
    fn dirty(w: u16, h: u16) -> Buffer {
        let mut buf = Buffer::empty(ratatui::layout::Rect::new(0, 0, w, h));
        let loud = Style::new()
            .fg(Color::Red)
            .bg(Color::Green)
            .add_modifier(Modifier::UNDERLINED | Modifier::BOLD | Modifier::REVERSED);
        for y in 0..h {
            for x in 0..w {
                let cell = buf.cell_mut((x, y)).unwrap();
                cell.set_symbol("X");
                cell.set_style(loud);
            }
        }
        buf
    }

    #[test]
    fn painting_over_a_pane_does_not_inherit_its_attributes() {
        // `Cell::set_style` merges modifiers rather than replacing them, so a
        // float drawn over an underlined `ls` listing used to keep the
        // underline. Anything that paints a cell has to reset it first.
        let mut buf = dirty(20, 6);
        let rect = Rect::new(4, 1, 12, 4);
        let cfg = Appearance::default();
        let parser = vt100::Parser::new(2, 10, 0);
        draw_border(&mut buf, rect, "float", true, false, false, &cfg);
        draw_screen(&mut buf, rect.shrink(1), parser.screen(), false);

        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                let cell = buf.cell((x, y)).unwrap();
                assert!(
                    !cell.modifier.contains(Modifier::UNDERLINED)
                        && !cell.modifier.contains(Modifier::REVERSED),
                    "stale attributes at {x},{y}: {:?}",
                    cell.modifier
                );
                assert_ne!(cell.symbol(), "X", "pane text left at {x},{y}");
            }
        }
    }

    #[test]
    fn clear_wipes_attributes_too() {
        let mut buf = dirty(8, 3);
        clear(
            &mut buf,
            Rect::new(0, 0, 8, 3),
            Style::new().bg(Color::Black),
        );
        for y in 0..3 {
            for x in 0..8 {
                let cell = buf.cell((x, y)).unwrap();
                assert_eq!(cell.modifier, Modifier::empty());
                assert_eq!(cell.symbol(), " ");
            }
        }
    }
    use super::*;

    fn buffer(w: u16, h: u16) -> Buffer {
        Buffer::empty(ratatui::layout::Rect::new(0, 0, w, h))
    }

    fn sym(buf: &Buffer, x: u16, y: u16) -> String {
        buf[(x, y)].symbol().to_string()
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (buf.area.x..buf.area.right())
            .map(|x| sym(buf, x, y))
            .collect()
    }

    fn plain() -> Appearance {
        Appearance {
            title_position: TitlePosition::Hidden,
            ..Appearance::default()
        }
    }

    #[test]
    fn curved_corners_and_edges() {
        let mut buf = buffer(10, 4);
        draw_border(
            &mut buf,
            Rect::new(0, 0, 10, 4),
            "",
            false,
            false,
            false,
            &plain(),
        );
        assert_eq!(row(&buf, 0), "╭────────╮");
        assert_eq!(row(&buf, 3), "╰────────╯");
        assert_eq!(sym(&buf, 0, 1), "│");
        assert_eq!(sym(&buf, 9, 2), "│");
        assert_eq!(sym(&buf, 5, 1), " ", "interior untouched");
    }

    #[test]
    fn every_style_has_its_corner() {
        for (style, tl) in [
            (BorderStyle::Curved, "╭"),
            (BorderStyle::Square, "┌"),
            (BorderStyle::Heavy, "┏"),
            (BorderStyle::Double, "╔"),
            (BorderStyle::Dashed, "╭"),
        ] {
            let cfg = Appearance {
                border_style: style,
                ..plain()
            };
            let mut buf = buffer(6, 3);
            draw_border(
                &mut buf,
                Rect::new(0, 0, 6, 3),
                "",
                false,
                false,
                false,
                &cfg,
            );
            assert_eq!(sym(&buf, 0, 0), tl, "{style:?}");
        }
        assert_eq!(frame_chars(BorderStyle::Heavy).h, '━');
        assert_eq!(frame_chars(BorderStyle::Dashed).v, '┆');
    }

    #[test]
    fn style_none_draws_nothing() {
        let cfg = Appearance {
            border_style: BorderStyle::None,
            ..plain()
        };
        let mut buf = buffer(6, 3);
        draw_border(
            &mut buf,
            Rect::new(0, 0, 6, 3),
            "sh",
            true,
            false,
            true,
            &cfg,
        );
        assert_eq!(row(&buf, 0), "      ");
        assert_eq!(frame_chars(BorderStyle::None).tl, ' ');
    }

    #[test]
    fn title_sits_in_the_top_edge_and_truncates() {
        let cfg = Appearance::default();
        let mut buf = buffer(14, 3);
        draw_border(
            &mut buf,
            Rect::new(0, 0, 14, 3),
            "zsh",
            false,
            false,
            false,
            &cfg,
        );
        assert_eq!(row(&buf, 0), "╭─ zsh ──────╮");

        let mut buf = buffer(12, 3);
        draw_border(
            &mut buf,
            Rect::new(0, 0, 12, 3),
            "a-long-name",
            false,
            false,
            false,
            &cfg,
        );
        assert_eq!(row(&buf, 0), "╭─ a-lon… ─╮");
    }

    #[test]
    fn wide_title_does_not_overflow() {
        let cfg = Appearance::default();
        let mut buf = buffer(14, 3);
        draw_border(
            &mut buf,
            Rect::new(0, 0, 14, 3),
            "世界世界世界",
            false,
            false,
            false,
            &cfg,
        );
        // Right corner survives and the row is still exactly 14 columns wide.
        assert_eq!(sym(&buf, 13, 0), "╮");
        assert_eq!(sym(&buf, 12, 0), "─");
        assert_eq!(row(&buf, 0), "╭─ 世界世… ──╮");
    }

    #[test]
    fn title_position_bottom_and_hidden() {
        let bottom = Appearance {
            title_position: TitlePosition::Bottom,
            ..Appearance::default()
        };
        let mut buf = buffer(12, 3);
        draw_border(
            &mut buf,
            Rect::new(0, 0, 12, 3),
            "zsh",
            false,
            false,
            false,
            &bottom,
        );
        assert_eq!(row(&buf, 0), "╭──────────╮");
        assert_eq!(row(&buf, 2), "╰─ zsh ────╯");

        let mut buf = buffer(12, 3);
        draw_border(
            &mut buf,
            Rect::new(0, 0, 12, 3),
            "zsh",
            false,
            false,
            false,
            &plain(),
        );
        assert_eq!(row(&buf, 0), "╭──────────╮");
        assert_eq!(row(&buf, 2), "╰──────────╯");
    }

    #[test]
    fn colour_follows_focus_and_alert() {
        let cfg = Appearance::default();
        let fg = |focused, alert| {
            let mut buf = buffer(6, 3);
            draw_border(
                &mut buf,
                Rect::new(0, 0, 6, 3),
                "",
                focused,
                alert,
                false,
                &cfg,
            );
            (buf[(0u16, 0u16)].style().fg, buf[(0u16, 0u16)].style())
        };
        assert_eq!(fg(false, false).0, Some(cfg.border.into()));
        assert_eq!(fg(true, false).0, Some(cfg.border_focused.into()));
        assert_eq!(fg(true, true).0, Some(cfg.border_alert.into()));
        assert_eq!(fg(false, true).0, Some(cfg.border_alert.into()));
        assert!(fg(true, false).1.add_modifier.contains(Modifier::BOLD));
        assert!(!fg(false, false).1.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn zoom_marker_only_when_zoomed_and_when_it_fits() {
        let cfg = Appearance::default();
        let mut buf = buffer(40, 3);
        draw_border(
            &mut buf,
            Rect::new(0, 0, 40, 3),
            "zsh",
            false,
            false,
            false,
            &cfg,
        );
        assert!(!row(&buf, 0).contains('⛶'));

        let mut buf = buffer(40, 3);
        draw_border(
            &mut buf,
            Rect::new(0, 0, 40, 3),
            "zsh",
            false,
            false,
            true,
            &cfg,
        );
        assert_eq!(sym(&buf, 36, 0), "⛶");
        assert_eq!(sym(&buf, 37, 0), " ");
        assert_eq!(sym(&buf, 39, 0), "╮");

        // Too narrow next to the title: dropped rather than overlapping.
        let mut buf = buffer(12, 3);
        draw_border(
            &mut buf,
            Rect::new(0, 0, 12, 3),
            "abcdefg",
            false,
            false,
            true,
            &cfg,
        );
        assert!(!row(&buf, 0).contains('⛶'), "{}", row(&buf, 0));
    }

    #[test]
    fn out_of_bounds_rect_does_not_panic() {
        let mut buf = buffer(10, 5);
        let cfg = Appearance::default();
        draw_border(
            &mut buf,
            Rect::new(6, 3, 20, 20),
            "title",
            true,
            false,
            true,
            &cfg,
        );
        draw_border(
            &mut buf,
            Rect::new(60, 60, 10, 10),
            "t",
            false,
            false,
            false,
            &cfg,
        );
        draw_border(
            &mut buf,
            Rect::new(0, 0, 1, 1),
            "t",
            false,
            false,
            false,
            &cfg,
        );
        assert_eq!(sym(&buf, 6, 3), "╭");
    }

    #[test]
    fn screen_text_and_colour_are_copied() {
        let mut p = vt100::Parser::new(3, 8, 0);
        p.process(b"\x1b[31mhi");
        let mut buf = buffer(8, 3);
        draw_screen(&mut buf, Rect::new(0, 0, 8, 3), p.screen(), false);
        assert_eq!(row(&buf, 0), "hi      ");
        assert_eq!(buf[(0u16, 0u16)].style().fg, Some(Color::Indexed(1)));
        assert_eq!(buf[(4u16, 0u16)].style().fg, Some(Color::Reset));

        let mut buf = buffer(8, 3);
        draw_screen(&mut buf, Rect::new(0, 0, 8, 3), p.screen(), true);
        assert!(buf[(0u16, 0u16)]
            .style()
            .add_modifier
            .contains(Modifier::DIM));
    }

    #[test]
    fn wide_char_takes_two_cells_without_shifting() {
        let mut p = vt100::Parser::new(2, 8, 0);
        p.process("a世b".as_bytes());
        let mut buf = buffer(8, 2);
        draw_screen(&mut buf, Rect::new(0, 0, 8, 2), p.screen(), false);
        assert_eq!(sym(&buf, 0, 0), "a");
        assert_eq!(sym(&buf, 1, 0), "世");
        assert_eq!(sym(&buf, 2, 0), "");
        assert_eq!(sym(&buf, 3, 0), "b");
    }

    #[test]
    fn screen_clips_into_a_smaller_rect() {
        let mut p = vt100::Parser::new(5, 20, 0);
        p.process(b"abcdefghij\r\nklmno");
        let mut buf = buffer(10, 4);
        draw_screen(&mut buf, Rect::new(4, 1, 4, 2), p.screen(), false);
        assert_eq!(row(&buf, 1), "    abcd  ");
        assert_eq!(row(&buf, 2), "    klmn  ");
        assert_eq!(row(&buf, 0), "          ");
    }

    #[test]
    fn shadow_dims_the_band_only() {
        let mut buf = buffer(10, 6);
        let rect = Rect::new(1, 1, 5, 3);
        draw_border(&mut buf, rect, "", false, false, false, &plain());
        draw_shadow(&mut buf, rect);
        let dimmed = |x, y| buf[(x, y)].style().add_modifier.contains(Modifier::DIM);
        assert!(dimmed(6, 2), "right band");
        assert!(dimmed(6, 4), "corner");
        assert!(dimmed(2, 4), "bottom band");
        assert!(!dimmed(5, 3), "pane border untouched");
        assert!(!dimmed(6, 1), "band starts one row down");
        assert_eq!(sym(&buf, 6, 2), " ", "symbol preserved");
        assert_eq!(buf[(6u16, 2u16)].style().bg, Some(Color::Rgb(10, 10, 14)));
    }
}
