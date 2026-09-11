//! Terminal resize. `Layout::set_area` is all that runs on SIGWINCH (the app
//! calls it per tab, then pushes the new sizes to the ptys), so every geometry
//! invariant the rest of the app relies on has to survive it here.

use ttmux::action::Dir;
use ttmux::layout::{Layout, Mode, PaneId, Rect};

/// Mirrors the private `MIN` in layout.rs: smallest pane, borders included.
const MIN: u16 = 3;

fn free(area: Rect, n: u32) -> Layout {
    let mut l = Layout::new(area);
    for id in 1..=n {
        l.insert(id, None, None);
    }
    l.set_mode(Mode::Free);
    l
}

/// Tiling covers `area` exactly: no gaps, no overlaps, nothing escapes.
fn assert_exact(l: &Layout) {
    let g = l.geometry();
    let total: u32 = g.iter().map(|(_, r)| r.w as u32 * r.h as u32).sum();
    let area = l.area();
    assert_eq!(total, area.w as u32 * area.h as u32, "coverage: {g:?}");
    for (i, (_, a)) in g.iter().enumerate() {
        assert!(
            a.x >= area.x
                && a.y >= area.y
                && a.right() <= area.right()
                && a.bottom() <= area.bottom(),
            "{a:?} escapes {area:?}"
        );
        for (_, b) in g.iter().skip(i + 1) {
            assert!(!a.intersects(b), "{a:?} overlaps {b:?}");
        }
    }
}

/// No pane is lost, and every float keeps `MIN` cells (or the whole area, if it
/// is smaller) grabbable inside it. Floats may hang off the right/bottom — that
/// is the point of free mode — but never so far that they cannot be grabbed
/// back. Tiled panes are exempt: the tree divides whatever rows exist, so on a
/// tiny area they are legitimately thinner than MIN (`assert_exact` covers them).
fn assert_reachable(l: &Layout, ids: &[PaneId]) {
    let a = l.area();
    assert_eq!(l.ids(), ids, "pane lost by resize to {a:?}");
    for &id in ids {
        let r = l.rect_of(id).unwrap_or_else(|| panic!("no rect for {id}"));
        if !l.is_floating(id) {
            continue;
        }
        assert!(r.w > 0 && r.h > 0, "float {id} collapsed: {r:?}");
        let vis_w = r.right().min(a.right()).saturating_sub(r.x.max(a.x));
        let vis_h = r.bottom().min(a.bottom()).saturating_sub(r.y.max(a.y));
        assert!(
            vis_w >= MIN.min(a.w) && vis_h >= MIN.min(a.h),
            "float {id} at {r:?} is unreachable in {a:?}"
        );
    }
}

#[test]
fn free_shrink_keeps_every_pane() {
    let mut l = free(Rect::new(0, 0, 120, 40), 4);
    l.move_pane(4, Dir::Right, 500); // parked overhanging the right edge
    l.move_pane(4, Dir::Down, 500); // ... and the bottom
    let ids = l.ids();

    for area in [Rect::new(0, 0, 30, 10), Rect::new(0, 0, 4, 3)] {
        l.set_area(area);
        assert_reachable(&l, &ids);
    }
}

#[test]
fn free_shrink_then_grow_lands_back() {
    let mut l = free(Rect::new(0, 0, 120, 40), 4);
    let before = l.geometry();

    l.set_area(Rect::new(0, 0, 60, 20));
    l.set_area(Rect::new(0, 0, 120, 40));

    // Halving floors each coordinate, doubling cannot recover the dropped odd
    // cell: exactly 1 per axis, per direction. Nothing here overhangs, so
    // clamping adds no further error.
    for (id, was) in before {
        let now = l.rect_of(id).unwrap();
        for (a, b, what) in [
            (was.x, now.x, "x"),
            (was.y, now.y, "y"),
            (was.w, now.w, "w"),
            (was.h, now.h, "h"),
        ] {
            assert!(
                a.abs_diff(b) <= 1,
                "pane {id} {what}: {was:?} -> {now:?} after round trip"
            );
        }
    }
}

#[test]
fn degenerate_areas_do_not_panic() {
    // Debug builds panic on u16 overflow, so this is a real check, not a smoke test.
    let mut l = free(Rect::new(0, 0, 120, 40), 3);
    let ids = l.ids();
    for area in [
        Rect::new(0, 0, 1, 1),
        Rect::new(0, 0, 0, 10),
        Rect::new(0, 0, 10, 0),
        Rect::new(0, 0, 1, 400),
        Rect::new(0, 0, 400, 1),
        // A zero-sized area carries no proportion to scale the floats by; the
        // next real size still has to pull them back inside itself.
        Rect::new(0, 0, 0, 0),
        Rect::new(2, 1, 80, 24),
    ] {
        l.set_area(area);
        l.geometry();
        assert_eq!(l.ids(), ids, "pane lost at {area:?}");
    }
    assert_reachable(&l, &ids);

    // Same walk in tiling mode, where geometry() is recomputed from the tree.
    let mut t = Layout::new(Rect::new(0, 0, 120, 40));
    for id in 1..=5 {
        t.insert(id, None, None);
    }
    for area in [
        Rect::new(0, 0, 1, 1),
        Rect::new(0, 0, 0, 0),
        Rect::new(2, 1, 3, 400),
        Rect::new(0, 0, 80, 24),
    ] {
        t.set_area(area);
        assert_exact(&t);
    }
}

#[test]
fn tiling_resize_preserves_ratios_and_exactness() {
    let mut l = Layout::new(Rect::new(0, 0, 120, 40));
    l.insert(1, None, None);
    l.insert(2, Some(1), Some(Dir::Right));
    l.insert(3, Some(1), Some(Dir::Down));
    l.insert(4, Some(2), Some(Dir::Down));
    l.resize(1, Dir::Right, 20); // off-centre divider, so a lost ratio shows up

    let fracs: Vec<(PaneId, f32, f32)> = l
        .geometry()
        .iter()
        .map(|(id, r)| (*id, r.w as f32 / 120.0, r.h as f32 / 40.0))
        .collect();

    for area in [Rect::new(0, 0, 60, 20), Rect::new(3, 2, 200, 60)] {
        l.set_area(area);
        assert_exact(&l);
        for &(id, fw, fh) in &fracs {
            let r = l.rect_of(id).unwrap();
            let (nw, nh) = (r.w as f32 / area.w as f32, r.h as f32 / area.h as f32);
            // One cell of rounding on the smallest axis we resize to (20 rows).
            assert!((fw - nw).abs() < 0.06, "pane {id} width {fw} -> {nw}");
            assert!((fh - nh).abs() < 0.06, "pane {id} height {fh} -> {nh}");
        }
    }
}

#[test]
fn explicit_float_survives_resize() {
    let mut l = Layout::new(Rect::new(0, 0, 120, 40));
    for id in 1..=3 {
        l.insert(id, None, None);
    }
    l.toggle_float(2); // floats while the layout stays tiling
    assert!(l.is_floating(2));

    l.set_area(Rect::new(0, 0, 40, 12));
    assert_eq!(l.mode, Mode::Tiling);
    assert!(l.is_floating(2), "float docked itself on resize");
    assert_reachable(&l, &l.ids());
    // The tiled remainder still tiles exactly; the float is drawn over it.
    let a = l.area();
    let tiled: u32 = l
        .geometry()
        .iter()
        .filter(|(id, _)| *id != 2)
        .map(|(_, r)| r.w as u32 * r.h as u32)
        .sum();
    assert_eq!(tiled, a.w as u32 * a.h as u32);
}

#[test]
fn zoomed_pane_fills_the_new_area() {
    let mut l = free(Rect::new(0, 0, 120, 40), 3);
    l.set_zoom(Some(2));
    for area in [Rect::new(0, 0, 30, 10), Rect::new(5, 2, 200, 60)] {
        l.set_area(area);
        assert_eq!(l.geometry(), vec![(2, area)]);
        assert_eq!(l.rect_of(2), Some(area));
    }
}

#[test]
fn random_resizes_keep_invariants() {
    // Deterministic LCG (numerical recipes); a dependency for 4 lines is silly.
    let mut seed: u32 = 0x5eed;
    let mut rng = move || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        seed >> 16
    };

    for mode in [Mode::Tiling, Mode::Free] {
        let mut l = Layout::new(Rect::new(0, 0, 120, 40));
        for id in 1..=5 {
            l.insert(id, None, None);
        }
        l.set_mode(mode);
        l.move_pane(5, Dir::Right, 500); // an overhanging float, in free mode
        let ids = l.ids();

        for _ in 0..200 {
            let area = Rect::new(
                (rng() % 4) as u16,
                (rng() % 3) as u16,
                (rng() % 200) as u16,
                (rng() % 60) as u16,
            );
            l.set_area(area);
            if mode == Mode::Tiling {
                assert_exact(&l);
            }
            if area.w > 0 && area.h > 0 {
                assert_reachable(&l, &ids);
            }
        }
    }
}
