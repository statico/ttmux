//! Golden-ish rendering tests: build a scene the way the app does and assert
//! on the exact characters. These are what catch "the borders went wonky".

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as RRect;

use ttmux::action::Dir;
use ttmux::agent::AgentState;
use ttmux::config::{BorderStyle, Config};
use ttmux::layout::{Layout, Rect};
use ttmux::{render, status};

/// The buffer's contents as lines of text, trailing spaces trimmed.
fn text(buf: &Buffer) -> Vec<String> {
    (0..buf.area.height)
        .map(|y| {
            let row: String = (0..buf.area.width)
                .map(|x| {
                    buf.cell((x, y))
                        .map(|c| c.symbol().to_string())
                        .unwrap_or_else(|| " ".into())
                })
                .collect();
            row.trim_end().to_string()
        })
        .collect()
}

/// Draw a whole tab the way `app::draw` does: borders for every tile, then the
/// status bar along the bottom.
fn scene(cfg: &Config, layout: &Layout, focus: u32, w: u16, h: u16) -> Buffer {
    let mut buf = Buffer::empty(RRect::new(0, 0, w, h));
    for (id, outer) in layout.geometry() {
        render::draw_border(
            &mut buf,
            outer.shrink(cfg.appearance.gap),
            &format!("pane {id}"),
            id == focus,
            false,
            false,
            &cfg.appearance,
        );
    }
    if cfg.status.footer.enabled {
        let ctx = status::Ctx {
            session: "main",
            mode: "tiling",
            zoomed: false,
            tabs: &[("1:sh".into(), true)],
            panes: &[("sh".into(), AgentState::Idle)],
            alerts: 0,
            message: None,
            pending_prefix: false,
        };
        status::draw(
            &mut buf,
            Rect::new(0, h - 1, w, 1),
            &cfg.status,
            &cfg.status.footer,
            &ctx,
        );
    }
    buf
}

fn two_panes() -> Layout {
    let mut l = Layout::new(Rect::new(0, 0, 40, 9));
    l.insert(1, None, None);
    l.insert(2, Some(1), Some(Dir::Right));
    l
}

#[test]
fn curved_borders_look_like_curved_borders() {
    let cfg = Config::default();
    let lines = text(&scene(&cfg, &two_panes(), 1, 40, 10));
    assert_eq!(
        lines[0],
        "╭─ pane 1 ─────────╮╭─ pane 2 ─────────╮",
        "top edge\nfull render:\n{}",
        lines.join("\n")
    );
    assert_eq!(lines[1], "│                  ││                  │");
    assert_eq!(lines[8], "╰──────────────────╯╰──────────────────╯");
}

#[test]
fn each_border_style_renders_its_own_corners() {
    let mut cfg = Config::default();
    for (style, corner) in [
        (BorderStyle::Curved, '╭'),
        (BorderStyle::Square, '┌'),
        (BorderStyle::Heavy, '┏'),
        (BorderStyle::Double, '╔'),
        (BorderStyle::Dashed, '╭'),
    ] {
        cfg.appearance.border_style = style;
        let buf = scene(&cfg, &two_panes(), 1, 40, 10);
        assert_eq!(
            buf.cell((0u16, 0u16)).unwrap().symbol(),
            corner.to_string(),
            "{style:?}"
        );
    }
    cfg.appearance.border_style = BorderStyle::None;
    let buf = scene(&cfg, &two_panes(), 1, 40, 10);
    assert_eq!(buf.cell((0u16, 0u16)).unwrap().symbol(), " ");
}

#[test]
fn the_status_bar_fills_its_row_and_names_the_session() {
    let cfg = Config::default();
    let buf = scene(&cfg, &two_panes(), 1, 40, 10);
    let bar = &text(&buf)[9];
    assert!(bar.contains("main"), "no session in {bar:?}");
    assert!(bar.contains("1:sh"), "no tab in {bar:?}");
    // Every cell of the bar row is painted: the bar background, or the accent
    // behind the active tab.
    let painted = [Some(cfg.status.bg.into()), Some(cfg.status.accent.into())];
    for x in 0..40u16 {
        let bg = buf.cell((x, 9u16)).unwrap().style().bg;
        assert!(
            painted.contains(&bg),
            "column {x} of the status bar is unstyled: {bg:?}"
        );
    }
}

#[test]
fn a_gap_inserts_blank_columns_between_panes() {
    let mut cfg = Config::default();
    cfg.appearance.gap = 1;
    let lines = text(&scene(&cfg, &two_panes(), 1, 40, 10));
    // With a one-cell gap the two panes no longer touch.
    assert!(
        !lines[1].contains("││"),
        "panes still touching: {:?}",
        lines[1]
    );
}

#[test]
fn tiny_terminals_do_not_panic() {
    let cfg = Config::default();
    for (w, h) in [(1, 1), (2, 2), (3, 1), (10, 3), (200, 2)] {
        let mut l = Layout::new(Rect::new(0, 0, w, h.max(1)));
        l.insert(1, None, None);
        l.insert(2, Some(1), Some(Dir::Down));
        let _ = scene(&cfg, &l, 1, w, h);
    }
}

#[test]
fn every_status_widget_renders_without_panicking() {
    let mut cfg = Config::default();
    cfg.status.footer.left = status::WIDGETS.iter().map(|s| s.to_string()).collect();
    cfg.status.footer.center = vec![];
    cfg.status.footer.right = vec![];
    let buf = scene(&cfg, &two_panes(), 1, 120, 10);
    let bar = &text(&buf)[9];
    assert!(!bar.is_empty());
    // An unknown widget is surfaced rather than silently dropped.
    cfg.status.footer.left = vec!["bogus".into()];
    let buf = scene(&cfg, &two_panes(), 1, 40, 10);
    assert!(text(&buf)[9].contains("?bogus"));
}

#[test]
fn dragging_a_divider_keeps_the_tiling_exact() {
    let mut l = Layout::new(Rect::new(0, 0, 80, 24));
    l.insert(1, None, None);
    l.insert(2, Some(1), Some(Dir::Right));

    let divider = l.rect_of(1).unwrap().right();
    assert!(l.drag_start(divider, 10), "no divider at column {divider}");
    l.drag_to(divider - 14, 10);
    l.drag_end();

    let geo = l.geometry();
    let a = geo.iter().find(|(id, _)| *id == 1).unwrap().1;
    let b = geo.iter().find(|(id, _)| *id == 2).unwrap().1;
    assert_eq!(a.x, 0);
    assert_eq!(b.x, a.right(), "panes overlap or leave a gap: {a:?} {b:?}");
    assert_eq!(a.w + b.w, 80, "tiling no longer covers the area");
    assert!(a.w < 40, "the divider did not move left: {a:?}");
}

#[test]
fn the_snap_preview_tints_the_half_without_erasing_it() {
    let cfg = Config::default();
    let l = two_panes();
    let mut buf = scene(&cfg, &l, 1, 40, 10);
    let half = Rect::new(0, 5, 20, 4);
    render::draw_snap_preview(&mut buf, half, cfg.status.accent.into());

    for y in half.y..half.bottom() {
        assert_eq!(
            buf.cell((half.x, y)).unwrap().style().bg,
            Some(cfg.status.accent.into()),
            "row {y} of the preview is not tinted"
        );
    }
    // The pane's own border is still legible through it, and the other half is
    // untouched.
    assert_eq!(buf.cell((0u16, 8u16)).unwrap().symbol(), "╰");
    assert_ne!(
        buf.cell((0u16, 1u16)).unwrap().style().bg,
        Some(cfg.status.accent.into())
    );
}

#[test]
fn a_floated_pane_can_be_grabbed_by_its_top_border() {
    let mut l = Layout::new(Rect::new(0, 0, 80, 24));
    l.insert(1, None, None);
    l.insert(2, Some(1), Some(Dir::Right));
    l.toggle_float(2);
    assert!(l.is_floating(2));

    let r = l.rect_of(2).unwrap();
    let (gx, gy) = (r.x + r.w / 2, r.y);
    assert_eq!(
        l.pane_at(gx, gy),
        Some(2),
        "topmost at the float's top border"
    );
    assert!(
        matches!(l.hit_test(gx, gy), Some((2, ttmux::layout::DragKind::Move))),
        "hit_test gave {:?}",
        l.hit_test(gx, gy)
    );

    // The app raises the pane under the cursor before starting the drag.
    l.raise(2);
    assert!(l.drag_start(gx, gy), "drag did not start");
    l.drag_to(gx, gy + 3);
    l.drag_end();
    assert_eq!(l.rect_of(2).unwrap().y, r.y + 3);
}

// -------------------------------------------------------------- modals

/// A buffer with every cell filled, the way a pane leaves it.
fn covered(w: u16, h: u16) -> Buffer {
    let mut buf = Buffer::empty(RRect::new(0, 0, w, h));
    for y in 0..h {
        for x in 0..w {
            buf.cell_mut((x, y)).unwrap().set_symbol("X");
        }
    }
    buf
}

fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
        .collect()
}

#[test]
fn the_modal_shadow_is_two_columns_wide_and_one_row_deep() {
    // Cells are about twice as tall as they are wide, so an even-looking
    // shadow needs two columns on the right for every row at the bottom.
    let cfg = Config::default();
    let rect = Rect::new(4, 3, 20, 8);
    let mut buf = covered(40, 16);
    let plain = buf.cell((0u16, 0u16)).unwrap().style();
    ttmux::app::modal(&mut buf, rect, "help", "any key to close", &cfg);

    let shaded = |b: &Buffer, x: u16, y: u16| b.cell((x, y)).unwrap().style() != plain;
    for y in rect.y + 1..=rect.bottom() {
        assert!(shaded(&buf, rect.right(), y), "no shadow at {y} col 1");
        assert!(shaded(&buf, rect.right() + 1, y), "no shadow at {y} col 2");
    }
    for x in rect.x + 1..=rect.right() + 1 {
        assert!(shaded(&buf, x, rect.bottom()), "no shadow under column {x}");
    }
    // A third column, and the row under the bottom one, are untouched.
    for y in rect.y..=rect.bottom() {
        assert!(!shaded(&buf, rect.right() + 2, y), "shadow too wide at {y}");
    }
    for x in rect.x..=rect.right() + 2 {
        assert!(
            !shaded(&buf, x, rect.bottom() + 1),
            "shadow too deep at {x}"
        );
    }
    // The shadow stays outside: the modal's own ground is one colour.
    let bg = buf.cell((rect.x + 2, rect.y + 2)).unwrap().style().bg;
    for y in rect.y + 1..rect.bottom() - 1 {
        for x in rect.x + 1..rect.right() - 1 {
            assert_eq!(
                buf.cell((x, y)).unwrap().style().bg,
                bg,
                "the shadow reached inside at {x},{y}"
            );
        }
    }
}

#[test]
fn a_modal_sits_on_its_own_ground_not_the_bar_background() {
    // The lift is what makes it read as raised rather than as one more pane.
    let cfg = Config::default();
    let rect = Rect::new(2, 2, 30, 8);
    let mut buf = covered(40, 16);
    ttmux::app::modal(&mut buf, rect, "help", "any key to close", &cfg);
    let bg = buf.cell((rect.x + 2, rect.y + 2)).unwrap().style().bg;
    assert!(bg.is_some(), "the modal ground is transparent");
    assert_ne!(bg, Some(cfg.status.bg.into()), "same ground as the bar");
    assert_ne!(
        bg,
        buf.cell((0u16, 0u16)).unwrap().style().bg,
        "same ground as the pane behind it"
    );
}

#[test]
fn the_hint_is_dropped_before_the_content_when_the_modal_is_tiny() {
    let cfg = Config::default();
    let hint = "any key to close";

    let mut buf = covered(40, 16);
    let rect = Rect::new(0, 0, 40, 10);
    let inner = ttmux::app::modal(&mut buf, rect, "help", hint, &cfg);
    assert!(row_text(&buf, inner.bottom()).contains(hint), "no hint");
    assert!(row_text(&buf, rect.y).contains("help"), "no title");
    assert!(
        inner.bottom() < rect.bottom() - 1,
        "the hint ate the border"
    );

    // Three rows leave one for content; the hint is what gives way.
    let mut buf = covered(40, 16);
    let rect = Rect::new(0, 0, 40, 3);
    let inner = ttmux::app::modal(&mut buf, rect, "help", hint, &cfg);
    assert_eq!(inner.h, 1, "content was traded away for the hint");
    let screen: String = (0..3).map(|y| row_text(&buf, y)).collect();
    assert!(!screen.contains(hint), "the hint overlapped the content");
}

#[test]
fn modal_content_is_inset_from_the_border() {
    let cfg = Config::default();
    let mut buf = covered(40, 16);
    let rect = Rect::new(2, 1, 30, 10);
    let inner = ttmux::app::modal(&mut buf, rect, "help", "any key to close", &cfg);
    // A cell of padding at the sides and a row under the title chip, so text
    // never sits against the rule.
    assert_eq!((inner.x, inner.y), (rect.x + 2, rect.y + 2));
    assert_eq!(inner.right(), rect.right() - 2);
}

#[test]
fn every_modal_draws_at_any_size_without_panicking() {
    let cfg = Config::default();
    let welcome = ttmux::onboarding::Welcome::new();
    let settings = ttmux::settings_ui::Settings::new();
    for (w, h) in [(0, 0), (1, 1), (2, 2), (4, 3), (8, 3), (13, 5), (80, 24)] {
        let mut buf = Buffer::empty(RRect::new(0, 0, w.max(1), h.max(1)));
        let rect = Rect::new(0, 0, w, h);
        for (title, hint) in [
            ("", ""),
            ("settings", "esc close"),
            ("x", &"k".repeat(200)[..]),
        ] {
            let inner = ttmux::app::modal(&mut buf, rect, title, hint, &cfg);
            welcome.draw(&mut buf, inner, &cfg);
        }
        settings.draw(&mut buf, rect, &cfg);
    }
}
