//! What ttmux's vt100 fork does that upstream did not. See vendor/vt100/TTMUX.md.

use vt100::{Color, Parser, UnderlineStyle};

fn screen(bytes: &str) -> Parser {
    let mut p = Parser::new(5, 20, 100);
    p.process(bytes.as_bytes());
    p
}

#[test]
fn emoji_clusters_take_one_wide_cell() {
    for (text, cluster) in [
        ("a\u{26a0}\u{fe0f}b", "\u{26a0}\u{fe0f}"),
        ("a👩\u{200d}💻b", "👩\u{200d}💻"),
        ("a👍🏽b", "👍🏽"),
        ("a🇺🇸b", "🇺🇸"),
        (
            "a👨\u{200d}👩\u{200d}👧\u{200d}👦b",
            "👨\u{200d}👩\u{200d}👧\u{200d}👦",
        ),
    ] {
        let p = screen(text);
        let s = p.screen();
        assert_eq!(s.cell(0, 1).unwrap().contents(), cluster, "{text:?}");
        assert!(s.cell(0, 1).unwrap().is_wide(), "{text:?}");
        assert!(s.cell(0, 2).unwrap().is_wide_continuation(), "{text:?}");
        assert_eq!(s.cell(0, 3).unwrap().contents(), "b", "{text:?}");
        assert_eq!(s.cursor_position(), (0, 4), "{text:?}");
    }
    // Two flags are two clusters, not one four-wide blob.
    let p = screen("🇺🇸🇫🇷");
    assert_eq!(p.screen().cell(0, 2).unwrap().contents(), "🇫🇷");
}

#[test]
fn a_region_pinned_to_the_top_still_feeds_scrollback() {
    // Codex's shape: a footer held in row 5, history scrolling above it.
    let mut p = screen("\x1b[1;4r\x1b[4;1H");
    for i in 0..10 {
        p.process(format!("line{i}\r\n").as_bytes());
    }
    p.screen_mut().set_scrollback(100);
    // Ten newlines at the region's bottom: three blank rows, then line0-6.
    assert_eq!(p.screen().scrollback(), 10);
    // A region lower down scrolls in place, as it always did.
    let mut p = screen("\x1b[2;4r\x1b[4;1H");
    for i in 0..10 {
        p.process(format!("line{i}\r\n").as_bytes());
    }
    p.screen_mut().set_scrollback(100);
    assert_eq!(p.screen().scrollback(), 0);
}

#[test]
fn csi_3j_clears_the_scrollback_only() {
    let mut p = screen("");
    for i in 0..10 {
        p.process(format!("line{i}\r\n").as_bytes());
    }
    p.process(b"\x1b[3J");
    p.screen_mut().set_scrollback(100);
    assert_eq!(p.screen().scrollback(), 0);
    assert!(p.screen().contents().contains("line9"));
}

#[test]
fn extended_sgr_is_kept_on_the_cell() {
    let p =
        screen("\x1b[4:3;58:2::255:0:0;9;5;8mA\x1b[4:0;59;29;25;28;21mB\x1b[38:2::1:2:3;48;5;7mC");
    let s = p.screen();
    let a = s.cell(0, 0).unwrap();
    assert_eq!(a.underline_style(), UnderlineStyle::Curly);
    assert_eq!(a.underline_color(), Color::Rgb(255, 0, 0));
    assert!(a.strikethrough() && a.blink() && a.hidden());
    let b = s.cell(0, 1).unwrap();
    assert_eq!(b.underline_style(), UnderlineStyle::Double);
    assert_eq!(b.underline_color(), Color::Default);
    assert!(!b.strikethrough() && !b.blink() && !b.hidden());
    let c = s.cell(0, 2).unwrap();
    assert_eq!(c.fgcolor(), Color::Rgb(1, 2, 3));
    assert_eq!(c.bgcolor(), Color::Idx(7));
    // The semicolon spelling of an underline colour eats its arguments:
    // upstream read the `2` as dim.
    let p = screen("\x1b[58;2;1;2;3mD");
    let d = p.screen().cell(0, 0).unwrap();
    assert!(!d.dim());
    assert_eq!(d.underline_color(), Color::Rgb(1, 2, 3));
}
