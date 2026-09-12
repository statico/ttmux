//! Pure geometry: the tiling tree, floating rects, mouse hit-testing.
//!
//! No I/O and no terminal here — everything is computed from an `area` plus a
//! binary split tree (ratio-based, so resizes survive terminal resizes) and a
//! list of absolutely-positioned floating panes.

use std::collections::HashMap;

use crate::action::Dir;

/// Stable identifier for a pane. Allocated by the app, opaque here.
pub type PaneId = u32;

/// Smallest pane any deliberate action creates, borders included: two border
/// cells plus one of content.
///
/// It is a floor on what `insert`, docking with `toggle_float` and every drag
/// will produce, not an invariant of `geometry()`. Presets and a shrinking
/// `area` divide whatever cells exist between the panes already there, so a
/// tiled pane can end up thinner than this — down to nothing on a one-cell
/// area. Only floats are kept `MIN` across, and only so they stay grabbable.
const MIN: u16 = 3;

/// A rectangle in terminal cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, w, h }
    }

    /// One past the last column.
    pub fn right(&self) -> u16 {
        self.x.saturating_add(self.w)
    }

    /// One past the last row.
    pub fn bottom(&self) -> u16 {
        self.y.saturating_add(self.h)
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// Saturating inset on all sides.
    pub fn shrink(&self, n: u16) -> Rect {
        let two = n.saturating_mul(2);
        Rect {
            x: self.x.saturating_add(n),
            y: self.y.saturating_add(n),
            w: self.w.saturating_sub(two),
            h: self.h.saturating_sub(two),
        }
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }
}

impl From<Rect> for ratatui::layout::Rect {
    fn from(r: Rect) -> Self {
        ratatui::layout::Rect::new(r.x, r.y, r.w, r.h)
    }
}

impl From<ratatui::layout::Rect> for Rect {
    fn from(r: ratatui::layout::Rect) -> Self {
        Rect::new(r.x, r.y, r.width, r.height)
    }
}

/// How panes are positioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Panes tile `area` exactly.
    Tiling,
    /// Panes are absolutely positioned and may overlap.
    Free,
}

/// Canned tiling arrangements. `Tree` means "whatever the user built".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    EvenHorizontal,
    EvenVertical,
    MainVertical,
    MainHorizontal,
    Tree,
}

impl Preset {
    pub fn as_str(self) -> &'static str {
        match self {
            Preset::EvenHorizontal => "even-horizontal",
            Preset::EvenVertical => "even-vertical",
            Preset::MainVertical => "main-vertical",
            Preset::MainHorizontal => "main-horizontal",
            Preset::Tree => "tree",
        }
    }
}

impl std::str::FromStr for Preset {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "even-horizontal" | "even-h" => Preset::EvenHorizontal,
            "even-vertical" | "even-v" => Preset::EvenVertical,
            "main-vertical" | "main-v" => Preset::MainVertical,
            "main-horizontal" | "main-h" => Preset::MainHorizontal,
            "tree" => Preset::Tree,
            other => return Err(format!("unknown preset: {other}")),
        })
    }
}

/// How wide the move grab on a tile's top border is.
///
/// `render::draw_border` writes the title at `x + 2` as ` title `, but layout
/// knows nothing about titles, so this is a fixed run instead of the real
/// width: big enough to be an easy target, small enough that most of the
/// border still drags the divider it doubles as.
const GRAB: u16 = 12;

/// What a mouse press grabbed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragKind {
    /// Move a floating pane.
    Move,
    /// Resize a floating pane by the named sides. Corners set two of them.
    Resize {
        left: bool,
        right: bool,
        top: bool,
        bottom: bool,
    },
    /// Move the divider of the n-th split node (pre-order index).
    Divider(usize),
    /// Where two perpendicular dividers cross: dragging moves both, which is
    /// what makes a tiled pane resizable by its corner and not only its edges.
    Corner { across: usize, down: usize },
    /// Drag a tiled pane by its title, to snap it into a half of another pane.
    Grab,
}

/// Split orientation. `Horizontal` puts the children side by side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
}

impl Axis {
    fn of(dir: Dir) -> Axis {
        match dir {
            Dir::Left | Dir::Right => Axis::Horizontal,
            Dir::Up | Dir::Down => Axis::Vertical,
        }
    }

    /// The side of `rect` a split on this axis divides.
    fn len_of(self, rect: Rect) -> u16 {
        match self {
            Axis::Horizontal => rect.w,
            Axis::Vertical => rect.h,
        }
    }
}

#[derive(Debug, Clone)]
enum Node {
    Leaf(PaneId),
    Split {
        dir: Axis,
        ratio: f32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

#[derive(Debug, Clone, Copy)]
struct Drag {
    id: PaneId,
    kind: DragKind,
    /// Pointer offset from the thing being dragged at press time.
    grab: (i32, i32),
    /// Rect of the float at press time; every resize delta is measured from
    /// it rather than from the current rect, so a drag back to the press point
    /// restores the original exactly. Unused for dividers and grabs.
    start: Rect,
    /// Where the pointer is now, for `snap_target`.
    at: (u16, u16),
}

/// The layout of one tab: a tiling tree plus floating panes.
#[derive(Debug, Clone)]
pub struct Layout {
    pub mode: Mode,
    pub preset: Preset,
    pub zoomed: Option<PaneId>,
    area: Rect,
    root: Option<Node>,
    /// Absolute rects for panes drawn free (explicit floats, or everything in `Free`).
    rects: HashMap<PaneId, Rect>,
    /// The same rects as fractions of `area` (x, y, w, h), written only when the
    /// user places a float. `set_area` derives `rects` from these instead of
    /// rescaling the last rounded rect, which would drift on every resize.
    desired: HashMap<PaneId, (f64, f64, f64, f64)>,
    /// Panes that float even in tiling mode (they are not in the tree).
    explicit: Vec<PaneId>,
    /// Z order of free-drawn panes, back to front.
    z: Vec<PaneId>,
    drag: Option<Drag>,
}

impl Layout {
    /// An empty layout covering `area`.
    pub fn new(area: Rect) -> Layout {
        Layout {
            mode: Mode::Tiling,
            preset: Preset::Tree,
            zoomed: None,
            area,
            root: None,
            rects: HashMap::new(),
            desired: HashMap::new(),
            explicit: Vec::new(),
            z: Vec::new(),
            drag: None,
        }
    }

    /// Resize the whole layout. Tiled ratios are kept; float rects scale with it.
    ///
    /// Floats are re-derived from the fractions in `desired`, never from the
    /// rect they currently have, so a shrink and a grow back land exactly where
    /// they started however many times it happens.
    pub fn set_area(&mut self, area: Rect) {
        self.area = area;
        let ids: Vec<PaneId> = self.rects.keys().copied().collect();
        for id in ids {
            // A degenerate area has no proportion to place a float in, so the
            // old rect stands; it still has to be clamped back inside `area` or
            // it stays off screen.
            let r = match self.desired.get(&id) {
                Some(&(fx, fy, fw, fh)) if area.w > 0 && area.h > 0 => Rect::new(
                    area.x.saturating_add(from_frac(fx, area.w)),
                    area.y.saturating_add(from_frac(fy, area.h)),
                    from_frac(fw, area.w).max(1),
                    from_frac(fh, area.h).max(1),
                ),
                _ => self.rects[&id],
            };
            let clamped = self.clamp_rect(r);
            self.rects.insert(id, clamped);
        }
    }

    pub fn area(&self) -> Rect {
        self.area
    }

    // ---------------------------------------------------------------- panes

    /// Add a pane, splitting `near` (or the last pane) along `dir`.
    ///
    /// `dir` of `None` halves the pane whichever way `default_axis` prefers.
    /// Returns false and changes nothing when the split would leave either
    /// half under `MIN` along the split axis, so the caller keeps its pane and
    /// reports "no room" rather than being handed one nobody can read.
    pub fn insert(&mut self, id: PaneId, near: Option<PaneId>, dir: Option<Dir>) -> bool {
        if self.ids().contains(&id) {
            return false;
        }
        let leaves = self.leaves();
        let near = near
            .filter(|n| leaves.contains(n))
            .or_else(|| leaves.last().copied());
        // `near` is only ever `None` when the tree is empty, so this is also
        // the "first pane" case.
        match near {
            None => self.root = Some(Node::Leaf(id)),
            Some(near) => {
                let rect = self.tiled_rect(near).unwrap_or(self.area);
                let axis = dir.map_or_else(|| default_axis(rect), Axis::of);
                if !splittable(rect, axis) {
                    return false;
                }
                let first = matches!(dir, Some(Dir::Left) | Some(Dir::Up));
                if let Some(root) = self.root.as_mut() {
                    split_leaf(root, near, id, axis, first);
                }
                self.preset = Preset::Tree;
            }
        }
        if self.mode == Mode::Free {
            // Give the newcomer the rect it would have had while tiled.
            let r = self
                .tiled_geometry()
                .into_iter()
                .find(|(p, _)| *p == id)
                .map_or_else(|| self.area.shrink(2), |(_, r)| r);
            self.put_rect(id, r);
            self.z.push(id);
        }
        true
    }

    /// Remove a pane; its sibling collapses into its place.
    pub fn remove(&mut self, id: PaneId) {
        if let Some(root) = self.root.take() {
            self.root = remove_leaf(root, id);
        }
        self.rects.remove(&id);
        self.desired.remove(&id);
        self.explicit.retain(|p| *p != id);
        self.z.retain(|p| *p != id);
        if self.zoomed == Some(id) {
            self.zoomed = None;
        }
        if self.drag.map(|d| d.id) == Some(id) {
            self.drag = None;
        }
    }

    /// All panes: the tree's leaves in tree order, then any explicit floats
    /// (which are not in the tree) back to front.
    pub fn ids(&self) -> Vec<PaneId> {
        let mut out = self.leaves();
        for id in &self.z {
            if !out.contains(id) {
                out.push(*id);
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none() && self.explicit.is_empty()
    }

    // ------------------------------------------------------------- geometry

    /// Draw order: tiled panes first, floating panes back to front.
    pub fn geometry(&self) -> Vec<(PaneId, Rect)> {
        if let Some(z) = self.zoomed {
            if self.ids().contains(&z) {
                return vec![(z, self.area)];
            }
        }
        self.all_rects()
    }

    pub fn rect_of(&self, id: PaneId) -> Option<Rect> {
        if self.zoomed == Some(id) {
            return Some(self.area);
        }
        self.raw_rect(id)
    }

    /// Topmost pane at a point.
    pub fn pane_at(&self, x: u16, y: u16) -> Option<PaneId> {
        if let Some(z) = self.zoomed {
            return if self.area.contains(x, y) && self.ids().contains(&z) {
                Some(z)
            } else {
                None
            };
        }
        for id in self.z.iter().rev() {
            if self.rects.get(id).is_some_and(|r| r.contains(x, y)) {
                return Some(*id);
            }
        }
        if self.mode == Mode::Tiling {
            for (id, r) in self.tiled_geometry() {
                if r.contains(x, y) {
                    return Some(id);
                }
            }
        }
        None
    }

    /// Adjacent pane in `dir`: nearest one with the largest perpendicular overlap.
    pub fn neighbor(&self, id: PaneId, dir: Dir) -> Option<PaneId> {
        let me = self.raw_rect(id)?;
        let mut best: Option<(u16, u16, PaneId)> = None;
        for (other, r) in self.all_rects() {
            if other == id {
                continue;
            }
            let (ok, dist, overlap) = match dir {
                Dir::Right => (
                    r.x > me.x,
                    r.x.saturating_sub(me.right()),
                    overlap(me.y, me.bottom(), r.y, r.bottom()),
                ),
                Dir::Left => (
                    r.x < me.x,
                    me.x.saturating_sub(r.right()),
                    overlap(me.y, me.bottom(), r.y, r.bottom()),
                ),
                Dir::Down => (
                    r.y > me.y,
                    r.y.saturating_sub(me.bottom()),
                    overlap(me.x, me.right(), r.x, r.right()),
                ),
                Dir::Up => (
                    r.y < me.y,
                    me.y.saturating_sub(r.bottom()),
                    overlap(me.x, me.right(), r.x, r.right()),
                ),
            };
            if !ok || overlap == 0 {
                continue;
            }
            let better = match best {
                None => true,
                Some((bd, bo, _)) => dist < bd || (dist == bd && overlap > bo),
            };
            if better {
                best = Some((dist, overlap, other));
            }
        }
        best.map(|(_, _, id)| id)
    }

    /// Next pane in `ids()` order, wrapping.
    pub fn next(&self, id: PaneId) -> Option<PaneId> {
        let ids = self.ids();
        let i = ids.iter().position(|p| *p == id)?;
        Some(ids[(i + 1) % ids.len()])
    }

    /// Previous pane in `ids()` order, wrapping.
    pub fn prev(&self, id: PaneId) -> Option<PaneId> {
        let ids = self.ids();
        let i = ids.iter().position(|p| *p == id)?;
        Some(ids[(i + ids.len() - 1) % ids.len()])
    }

    // -------------------------------------------------------------- changes

    /// Move the pane's edge `n` cells towards `dir`: for a float its right or
    /// bottom edge, for a tiled pane the nearest ancestor divider on that
    /// axis. Whether that grows or shrinks the pane depends on which side of
    /// the divider it sits.
    pub fn resize(&mut self, id: PaneId, dir: Dir, n: u16) {
        if n == 0 {
            return;
        }
        if let Some(r) = self.rects.get(&id).copied() {
            let grown = match dir {
                Dir::Right => Rect::new(r.x, r.y, r.w.saturating_add(n), r.h),
                Dir::Left => Rect::new(r.x, r.y, r.w.saturating_sub(n).max(MIN), r.h),
                Dir::Down => Rect::new(r.x, r.y, r.w, r.h.saturating_add(n)),
                Dir::Up => Rect::new(r.x, r.y, r.w, r.h.saturating_sub(n).max(MIN)),
            };
            self.put_rect(id, grown);
            return;
        }
        let axis = Axis::of(dir);
        let forward = matches!(dir, Dir::Right | Dir::Down);
        let Some(path) = self.leaf_path(id) else {
            return;
        };
        let splits = self.splits();
        // Nearest ancestor split on the matching axis: move its divider.
        for k in (0..path.len()).rev() {
            let ancestor = &path[..k];
            let Some(&(rect, ax, _)) = splits.iter().find(|(_, _, p)| p.as_slice() == ancestor)
            else {
                continue;
            };
            if ax != axis {
                continue;
            }
            let len = axis.len_of(rect);
            if len < 2 * MIN {
                continue;
            }
            let cur = part(len, self.ratio_at(ancestor));
            let next = if forward {
                cur.saturating_add(n)
            } else {
                cur.saturating_sub(n)
            }
            .clamp(MIN, len - MIN);
            self.set_ratio_at(ancestor, next as f32 / len as f32);
            self.preset = Preset::Tree;
            return;
        }
    }

    /// Move a floating pane by `n` cells. No-op for tiled panes.
    pub fn move_pane(&mut self, id: PaneId, dir: Dir, n: u16) {
        let Some(r) = self.rects.get(&id).copied() else {
            return;
        };
        let n = n as i32;
        let (dx, dy) = match dir {
            Dir::Left => (-n, 0),
            Dir::Right => (n, 0),
            Dir::Up => (0, -n),
            Dir::Down => (0, n),
        };
        let moved = Rect::new(
            (r.x as i32 + dx).max(0) as u16,
            (r.y as i32 + dy).max(0) as u16,
            r.w,
            r.h,
        );
        self.put_rect(id, moved);
    }

    /// Swap a pane with the next one in `ids()` order.
    pub fn swap_next(&mut self, id: PaneId) {
        let Some(other) = self.next(id) else { return };
        if other == id {
            return;
        }
        if let Some(root) = self.root.as_mut() {
            swap_leaves(root, id, other);
        }
        let a = self.rects.remove(&id);
        let b = self.rects.remove(&other);
        if let Some(b) = b {
            self.rects.insert(id, b);
        }
        if let Some(a) = a {
            self.rects.insert(other, a);
        }
        let da = self.desired.remove(&id);
        let db = self.desired.remove(&other);
        if let Some(db) = db {
            self.desired.insert(id, db);
        }
        if let Some(da) = da {
            self.desired.insert(other, da);
        }
        for p in self.z.iter_mut() {
            if *p == id {
                *p = other;
            } else if *p == other {
                *p = id;
            }
        }
        for p in self.explicit.iter_mut() {
            if *p == id {
                *p = other;
            } else if *p == other {
                *p = id;
            }
        }
    }

    /// Rebuild the tree in a canned arrangement, keeping pane order.
    pub fn set_preset(&mut self, p: Preset) {
        self.preset = p;
        let leaves = self.leaves();
        if p == Preset::Tree || leaves.is_empty() {
            return;
        }
        self.root = Some(build_preset(p, &leaves));
    }

    /// Switch mode. Tiling -> Free seeds float rects from the tiled geometry.
    pub fn set_mode(&mut self, m: Mode) {
        if m == self.mode {
            return;
        }
        match m {
            Mode::Free => {
                let tiled = self.tiled_geometry();
                let mut z: Vec<PaneId> = Vec::new();
                for (id, r) in tiled {
                    self.put_rect(id, r);
                    z.push(id);
                }
                // Explicit floats stay on top, in their existing order.
                for id in &self.z {
                    if !z.contains(id) {
                        z.push(*id);
                    }
                }
                self.z = z;
            }
            Mode::Tiling => {
                let explicit = self.explicit.clone();
                self.z.retain(|id| explicit.contains(id));
                let keep: Vec<PaneId> = self.z.clone();
                self.rects.retain(|id, _| keep.contains(id));
                self.desired.retain(|id, _| keep.contains(id));
            }
        }
        self.mode = m;
    }

    /// Float a tiled pane on top, or dock a floating one back into the tree.
    pub fn toggle_float(&mut self, id: PaneId) {
        if self.explicit.contains(&id) {
            let leaves = self.leaves();
            match leaves.last().copied() {
                None => self.root = Some(Node::Leaf(id)),
                Some(near) => {
                    let rect = self.raw_rect(near).unwrap_or(self.area);
                    let axis = default_axis(rect);
                    // Nowhere to dock it without making an unusable tile: leave
                    // it floating rather than wedge it in.
                    if !splittable(rect, axis) {
                        return;
                    }
                    if let Some(root) = self.root.as_mut() {
                        split_leaf(root, near, id, axis, false);
                    }
                }
            }
            self.explicit.retain(|p| *p != id);
            if self.mode == Mode::Tiling {
                self.z.retain(|p| *p != id);
                self.rects.remove(&id);
                self.desired.remove(&id);
            }
            self.preset = Preset::Tree;
            return;
        }
        let Some(rect) = self.raw_rect(id) else {
            return;
        };
        if let Some(root) = self.root.take() {
            self.root = remove_leaf(root, id);
        }
        // Don't land exactly on top of another float. Only cascade while the
        // nudge actually moves the rect: on a small area `clamp_rect` pins it
        // against the edge, and the loop would otherwise never make progress.
        let mut rect = self.clamp_rect(rect);
        while self
            .rects
            .iter()
            .any(|(p, r)| *p != id && *r == rect && self.z.contains(p))
        {
            let next = self.clamp_rect(Rect::new(
                rect.x.saturating_add(2),
                rect.y.saturating_add(1),
                rect.w,
                rect.h,
            ));
            if next == rect {
                break;
            }
            rect = next;
        }
        self.put_rect(id, rect);
        self.explicit.push(id);
        self.z.retain(|p| *p != id);
        self.z.push(id);
        self.preset = Preset::Tree;
    }

    pub fn is_floating(&self, id: PaneId) -> bool {
        self.explicit.contains(&id) || (self.mode == Mode::Free && self.z.contains(&id))
    }

    /// Bring a floating pane to the front.
    pub fn raise(&mut self, id: PaneId) {
        if self.z.contains(&id) {
            self.z.retain(|p| *p != id);
            self.z.push(id);
        }
    }

    /// Make one pane fill `area`, or clear the zoom with `None`.
    pub fn set_zoom(&mut self, id: Option<PaneId>) {
        self.zoomed = id.filter(|id| self.ids().contains(id));
    }

    // ----------------------------------------------------------------- mice

    /// What a press at `(x, y)` would grab.
    pub fn hit_test(&self, x: u16, y: u16) -> Option<(PaneId, DragKind)> {
        if self.zoomed.is_some() {
            return None;
        }
        for id in self.z.iter().rev() {
            let Some(r) = self.rects.get(id) else {
                continue;
            };
            if !r.contains(x, y) {
                continue;
            }
            let (left, right) = (x == r.x, x + 1 == r.right());
            let (top, bottom) = (y == r.y, y + 1 == r.bottom());
            // The top row is the title bar, so it moves — except at the two
            // corners, where resizing wins.
            let kind = if left || right || bottom {
                Some(DragKind::Resize {
                    left,
                    right,
                    top,
                    bottom,
                })
            } else if top {
                Some(DragKind::Move)
            } else {
                None
            };
            return kind.map(|k| (*id, k));
        }
        if self.mode == Mode::Tiling {
            if let Some((id, _)) = self.tiled_geometry().into_iter().find(|(_, r)| {
                y == r.y && x > r.x && x < r.right().saturating_sub(1).min(r.x + 1 + GRAB)
            }) {
                return Some((id, DragKind::Grab));
            }
            // Pre-order, so the outermost divider under the pointer comes
            // first. Only the splits on the chain of rects containing (x, y)
            // can match, and pre-order visits that chain outermost first.
            let mut hits: Vec<(usize, Axis, PaneId)> = vec![];
            for (i, (rect, axis, path)) in self.splits().into_iter().enumerate() {
                if !rect.contains(x, y) {
                    continue;
                }
                let first = part(axis.len_of(rect), self.ratio_at(&path));
                // The first child got no cells at all, so there is no boundary
                // between the two to grab — only the second child to click.
                if first == 0 {
                    continue;
                }
                // Both cells either side of the boundary grab it, so a divider
                // is two cells wide to the pointer.
                let on_edge = match axis {
                    Axis::Horizontal => x + 1 == rect.x + first || x == rect.x + first,
                    Axis::Vertical => y + 1 == rect.y + first || y == rect.y + first,
                };
                if on_edge {
                    let mut leaves = Vec::new();
                    collect_leaves(self.node_at(&path)?, &mut leaves);
                    if let Some(id) = leaves.first() {
                        hits.push((i, axis, *id));
                    }
                }
            }
            // A point on both a vertical and a horizontal divider is a corner,
            // and dragging it moves both — otherwise a tiled pane can only be
            // resized one axis at a time, which is not what a corner looks
            // like it does.
            let across = hits.iter().find(|(_, a, _)| *a == Axis::Horizontal);
            let down = hits.iter().find(|(_, a, _)| *a == Axis::Vertical);
            if let (Some(&(across, _, id)), Some(&(down, ..))) = (across, down) {
                return Some((id, DragKind::Corner { across, down }));
            }
            if let Some(&(i, _, id)) = hits.first() {
                return Some((id, DragKind::Divider(i)));
            }
        }
        None
    }

    /// While a tiled pane is being dragged: the pane it would land in, which
    /// side of it, and the half it would take (what the app previews).
    pub fn snap_target(&self) -> Option<(PaneId, Dir, Rect)> {
        let d = self.drag?;
        if d.kind != DragKind::Grab || self.mode != Mode::Tiling {
            return None;
        }
        let (x, y) = d.at;
        let target = self.pane_at(x, y)?;
        if target == d.id || !self.leaves().contains(&target) {
            return None;
        }
        let r = self.rect_of(target)?;
        // Nearest edge, as a fraction of the rect so a wide pane's left edge
        // does not always beat its top one.
        let (w, h) = (r.w.max(1) as f32, r.h.max(1) as f32);
        let dir = [
            (Dir::Left, (x - r.x) as f32 / w),
            (Dir::Right, (r.right() - 1 - x) as f32 / w),
            (Dir::Up, (y - r.y) as f32 / h),
            (Dir::Down, (r.bottom() - 1 - y) as f32 / h),
        ]
        .into_iter()
        .reduce(|a, b| if b.1 < a.1 { b } else { a })?
        .0;
        let (first, second) = split_rect(r, Axis::of(dir), 0.5);
        let half = if matches!(dir, Dir::Left | Dir::Up) {
            first
        } else {
            second
        };
        Some((target, dir, half))
    }

    /// Begin a drag. Returns false if nothing draggable is under the pointer.
    pub fn drag_start(&mut self, x: u16, y: u16) -> bool {
        let Some((id, kind)) = self.hit_test(x, y) else {
            self.drag = None;
            return false;
        };
        let start = self.rects.get(&id).copied().unwrap_or_default();
        let grab = match kind {
            // Offset from the float's origin; `drag_to` turns it into a delta.
            DragKind::Move | DragKind::Resize { .. } | DragKind::Grab => {
                (x as i32 - start.x as i32, y as i32 - start.y as i32)
            }
            DragKind::Divider(i) => match self.split_boundary(i) {
                Some((rect, Axis::Horizontal, _, first)) => (x as i32 - (rect.x + first) as i32, 0),
                Some((rect, Axis::Vertical, _, first)) => (0, y as i32 - (rect.y + first) as i32),
                None => (0, 0),
            },
            DragKind::Corner { across, down } => {
                let dx = self
                    .split_boundary(across)
                    .map_or(0, |(rect, _, _, first)| x as i32 - (rect.x + first) as i32);
                let dy = self
                    .split_boundary(down)
                    .map_or(0, |(rect, _, _, first)| y as i32 - (rect.y + first) as i32);
                (dx, dy)
            }
        };
        if matches!(kind, DragKind::Move | DragKind::Resize { .. }) {
            self.raise(id);
        }
        self.drag = Some(Drag {
            id,
            kind,
            grab,
            start,
            at: (x, y),
        });
        true
    }

    /// Continue a drag. Absolute coordinates; idempotent w.r.t. the press point.
    pub fn drag_to(&mut self, x: u16, y: u16) {
        let Some(d) = self.drag.as_mut() else { return };
        d.at = (x, y);
        let d = *d;
        match d.kind {
            DragKind::Move => {
                let nx = (x as i32 - d.grab.0).max(0) as u16;
                let ny = (y as i32 - d.grab.1).max(0) as u16;
                self.put_rect(d.id, Rect::new(nx, ny, d.start.w, d.start.h));
            }
            DragKind::Resize {
                left,
                right,
                top,
                bottom,
            } => {
                let s = d.start;
                let dx = x as i32 - (s.x as i32 + d.grab.0);
                let dy = y as i32 - (s.y as i32 + d.grab.1);
                let mut r = s;
                // Each side moves on its own; the clamps are what stop a window
                // turning inside out when dragged past the opposite side. The
                // `max(0)` matters when the float is already thinner than MIN
                // (an area narrower than MIN leaves it no choice): without it
                // the upper bound falls below the lower one and `clamp` panics.
                if right {
                    r.w = (s.w as i32 + dx).max(MIN as i32) as u16;
                }
                if left {
                    r.x = (s.x as i32 + dx).clamp(0, (s.right() as i32 - MIN as i32).max(0)) as u16;
                    r.w = s.right() - r.x;
                }
                if bottom {
                    r.h = (s.h as i32 + dy).max(MIN as i32) as u16;
                }
                if top {
                    r.y =
                        (s.y as i32 + dy).clamp(0, (s.bottom() as i32 - MIN as i32).max(0)) as u16;
                    r.h = s.bottom() - r.y;
                }
                self.put_rect(d.id, r);
            }
            // Nothing moves until the drop; `snap_target` follows the pointer.
            DragKind::Grab => {}
            DragKind::Divider(i) => {
                let Some((_, axis, _, _)) = self.split_boundary(i) else {
                    return;
                };
                let to = match axis {
                    Axis::Horizontal => x as i32 - d.grab.0,
                    Axis::Vertical => y as i32 - d.grab.1,
                };
                self.move_divider(i, to);
            }
            // Both axes at once. Moving `across` only changes ratios, never
            // the tree's shape, so `down` is still the same split afterwards.
            DragKind::Corner { across, down } => {
                self.move_divider(across, x as i32 - d.grab.0);
                self.move_divider(down, y as i32 - d.grab.1);
            }
        }
    }

    /// Finish a drag. A tiled pane dropped on a half of another one is
    /// re-parented there; everything else just stops.
    pub fn drag_end(&mut self) {
        if let (Some((target, dir, _)), Some(d)) = (self.snap_target(), self.drag) {
            // Re-parenting is remove + insert, and `insert` refuses a split
            // that would leave an unusable tile — so keep the old tree to put
            // back rather than dropping the pane on the floor.
            let before = self.root.clone();
            if let Some(root) = self.root.take() {
                self.root = remove_leaf(root, d.id);
            }
            if !self.insert(d.id, Some(target), Some(dir)) {
                self.root = before;
            }
        }
        self.drag = None;
    }

    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    // -------------------------------------------------------------- private

    fn leaves(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        if let Some(root) = self.root.as_ref() {
            collect_leaves(root, &mut out);
        }
        out
    }

    fn tiled_geometry(&self) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        if let Some(root) = self.root.as_ref() {
            collect_geometry(root, self.area, &mut out);
        }
        out
    }

    fn tiled_rect(&self, id: PaneId) -> Option<Rect> {
        self.tiled_geometry()
            .into_iter()
            .find(|(p, _)| *p == id)
            .map(|(_, r)| r)
    }

    /// Every pane's rect, ignoring zoom.
    fn all_rects(&self) -> Vec<(PaneId, Rect)> {
        let mut out = if self.mode == Mode::Tiling {
            self.tiled_geometry()
        } else {
            Vec::new()
        };
        for id in &self.z {
            if let Some(r) = self.rects.get(id) {
                out.push((*id, *r));
            }
        }
        out
    }

    fn raw_rect(&self, id: PaneId) -> Option<Rect> {
        self.all_rects()
            .into_iter()
            .find(|(p, _)| *p == id)
            .map(|(_, r)| r)
    }

    /// Split nodes in pre-order: rect, axis, path from the root.
    fn splits(&self) -> Vec<(Rect, Axis, Vec<bool>)> {
        let mut out = Vec::new();
        if let Some(root) = self.root.as_ref() {
            collect_splits(root, self.area, &mut Vec::new(), &mut out);
        }
        out
    }

    /// The node a `splits()` path leads to, or `None` if the tree changed
    /// under it.
    fn node_at(&self, path: &[bool]) -> Option<&Node> {
        let mut node = self.root.as_ref()?;
        for &step in path {
            let Node::Split { a, b, .. } = node else {
                return None;
            };
            node = if step { b } else { a };
        }
        Some(node)
    }

    fn ratio_at(&self, path: &[bool]) -> f32 {
        match self.node_at(path) {
            Some(Node::Split { ratio, .. }) => *ratio,
            _ => 0.5,
        }
    }

    fn set_ratio_at(&mut self, path: &[bool], value: f32) {
        let mut node = self.root.as_mut();
        for &step in path {
            match node {
                Some(Node::Split { a, b, .. }) => {
                    node = Some(if step { b } else { a });
                }
                _ => return,
            }
        }
        if let Some(Node::Split { ratio, .. }) = node {
            *ratio = value.clamp(0.01, 0.99);
        }
    }

    fn leaf_path(&self, id: PaneId) -> Option<Vec<bool>> {
        let mut path = Vec::new();
        let root = self.root.as_ref()?;
        if find_leaf(root, id, &mut path) {
            Some(path)
        } else {
            None
        }
    }

    /// Place a float, and remember where the user put it as a fraction of the
    /// area so `set_area` can reproduce it instead of rescaling a rounded rect.
    /// Split `i`'s rect, axis, path, and where its boundary currently sits
    /// measured from the rect's own origin.
    fn split_boundary(&self, i: usize) -> Option<(Rect, Axis, Vec<bool>, u16)> {
        let (rect, axis, path) = self.splits().get(i).cloned()?;
        let first = part(axis.len_of(rect), self.ratio_at(&path));
        Some((rect, axis, path, first))
    }

    /// Put split `i`'s boundary at absolute coordinate `to` on its own axis,
    /// keeping both children at least `MIN`.
    fn move_divider(&mut self, i: usize, to: i32) {
        let Some((rect, axis, path, _)) = self.split_boundary(i) else {
            return;
        };
        let len = axis.len_of(rect);
        if len < 2 * MIN {
            return;
        }
        let origin = if axis == Axis::Horizontal {
            rect.x
        } else {
            rect.y
        };
        let first = (to - origin as i32).clamp(MIN as i32, (len - MIN) as i32) as u16;
        self.set_ratio_at(&path, first as f32 / len as f32);
        self.preset = Preset::Tree;
    }

    fn put_rect(&mut self, id: PaneId, r: Rect) {
        let r = self.clamp_rect(r);
        let a = self.area;
        if a.w > 0 && a.h > 0 {
            self.desired.insert(
                id,
                (
                    (r.x - a.x) as f64 / a.w as f64,
                    (r.y - a.y) as f64 / a.h as f64,
                    r.w as f64 / a.w as f64,
                    r.h as f64 / a.h as f64,
                ),
            );
        }
        self.rects.insert(id, r);
    }

    /// Keep a rect inside `area`, at least MIN x MIN, or as much of the area
    /// as there is when it is smaller than that.
    fn clamp_rect(&self, mut r: Rect) -> Rect {
        let a = self.area;
        r.w = r.w.clamp(MIN.min(a.w).max(1), a.w.max(1));
        r.h = r.h.clamp(MIN.min(a.h).max(1), a.h.max(1));
        // A float may hang off the right or bottom edge — that is what makes
        // free mode feel like a window manager — but never so far that less
        // than `MIN` of it is still reachable. Unsigned coordinates mean it can
        // never hang off the left or top, so those stay flush with the area.
        let max_x = a.right().saturating_sub(MIN.min(r.w)).max(a.x);
        let max_y = a.bottom().saturating_sub(MIN.min(r.h)).max(a.y);
        r.x = r.x.max(a.x).min(max_x);
        r.y = r.y.max(a.y).min(max_y);
        r
    }
}

// ------------------------------------------------------------ free functions

fn overlap(a0: u16, a1: u16, b0: u16, b1: u16) -> u16 {
    a1.min(b1).saturating_sub(a0.max(b0))
}

/// A fraction of `len`, in whole cells.
fn from_frac(frac: f64, len: u16) -> u16 {
    (frac * len as f64).round() as u16
}

/// Which way to halve a rect when the caller did not say. Terminal cells are
/// roughly twice as tall as they are wide, so a rect only counts as wide
/// enough to split side by side once it is twice as wide as it is tall.
fn default_axis(rect: Rect) -> Axis {
    if rect.w / 2 >= rect.h {
        Axis::Horizontal
    } else {
        Axis::Vertical
    }
}

/// Is there room to split `rect` along `axis` into two usable panes?
fn splittable(rect: Rect, axis: Axis) -> bool {
    axis.len_of(rect) >= 2 * MIN
}

/// Size of the first child of a split, the remainder going to the second.
///
/// The three cases are the whole of the minimum-size policy for tiled panes.
/// With room for two usable children the ratio is honoured but held `MIN` off
/// either end. Below that there is nothing to honour: the cells are halved,
/// which is how a shrinking terminal produces tiles under `MIN` without ever
/// losing or double-counting a cell. Under two cells there is no split to
/// make, so the first child gets none and the second gets the lot.
fn part(len: u16, ratio: f32) -> u16 {
    if len < 2 {
        0
    } else if len < 2 * MIN {
        len / 2
    } else {
        ((len as f32 * ratio).round() as i32).clamp(MIN as i32, (len - MIN) as i32) as u16
    }
}

fn split_rect(rect: Rect, axis: Axis, ratio: f32) -> (Rect, Rect) {
    match axis {
        Axis::Horizontal => {
            let f = part(rect.w, ratio);
            (
                Rect::new(rect.x, rect.y, f, rect.h),
                Rect::new(rect.x + f, rect.y, rect.w - f, rect.h),
            )
        }
        Axis::Vertical => {
            let f = part(rect.h, ratio);
            (
                Rect::new(rect.x, rect.y, rect.w, f),
                Rect::new(rect.x, rect.y + f, rect.w, rect.h - f),
            )
        }
    }
}

fn collect_leaves(node: &Node, out: &mut Vec<PaneId>) {
    match node {
        Node::Leaf(id) => out.push(*id),
        Node::Split { a, b, .. } => {
            collect_leaves(a, out);
            collect_leaves(b, out);
        }
    }
}

fn collect_geometry(node: &Node, rect: Rect, out: &mut Vec<(PaneId, Rect)>) {
    match node {
        Node::Leaf(id) => out.push((*id, rect)),
        Node::Split { dir, ratio, a, b } => {
            let (ra, rb) = split_rect(rect, *dir, *ratio);
            collect_geometry(a, ra, out);
            collect_geometry(b, rb, out);
        }
    }
}

fn collect_splits(
    node: &Node,
    rect: Rect,
    path: &mut Vec<bool>,
    out: &mut Vec<(Rect, Axis, Vec<bool>)>,
) {
    if let Node::Split { dir, ratio, a, b } = node {
        out.push((rect, *dir, path.clone()));
        let (ra, rb) = split_rect(rect, *dir, *ratio);
        path.push(false);
        collect_splits(a, ra, path, out);
        path.pop();
        path.push(true);
        collect_splits(b, rb, path, out);
        path.pop();
    }
}

fn find_leaf(node: &Node, id: PaneId, path: &mut Vec<bool>) -> bool {
    match node {
        Node::Leaf(l) => *l == id,
        Node::Split { a, b, .. } => {
            path.push(false);
            if find_leaf(a, id, path) {
                return true;
            }
            path.pop();
            path.push(true);
            if find_leaf(b, id, path) {
                return true;
            }
            path.pop();
            false
        }
    }
}

fn split_leaf(node: &mut Node, near: PaneId, new: PaneId, axis: Axis, new_first: bool) -> bool {
    match node {
        Node::Leaf(l) if *l == near => {
            let (a, b) = if new_first {
                (Node::Leaf(new), Node::Leaf(near))
            } else {
                (Node::Leaf(near), Node::Leaf(new))
            };
            *node = Node::Split {
                dir: axis,
                ratio: 0.5,
                a: Box::new(a),
                b: Box::new(b),
            };
            true
        }
        Node::Leaf(_) => false,
        Node::Split { a, b, .. } => {
            split_leaf(a, near, new, axis, new_first) || split_leaf(b, near, new, axis, new_first)
        }
    }
}

fn remove_leaf(node: Node, id: PaneId) -> Option<Node> {
    match node {
        Node::Leaf(l) if l == id => None,
        Node::Leaf(l) => Some(Node::Leaf(l)),
        Node::Split { dir, ratio, a, b } => match (remove_leaf(*a, id), remove_leaf(*b, id)) {
            (Some(a), Some(b)) => Some(Node::Split {
                dir,
                ratio,
                a: Box::new(a),
                b: Box::new(b),
            }),
            (Some(x), None) | (None, Some(x)) => Some(x),
            (None, None) => None,
        },
    }
}

fn swap_leaves(node: &mut Node, x: PaneId, y: PaneId) {
    match node {
        Node::Leaf(l) => {
            if *l == x {
                *l = y;
            } else if *l == y {
                *l = x;
            }
        }
        Node::Split { a, b, .. } => {
            swap_leaves(a, x, y);
            swap_leaves(b, x, y);
        }
    }
}

/// Evenly chain `ids` along one axis: each split gives the head its fair share.
fn even(axis: Axis, ids: &[PaneId]) -> Node {
    match ids {
        [] => unreachable!("even() needs at least one pane"),
        [only] => Node::Leaf(*only),
        [head, rest @ ..] => Node::Split {
            dir: axis,
            ratio: 1.0 / ids.len() as f32,
            a: Box::new(Node::Leaf(*head)),
            b: Box::new(even(axis, rest)),
        },
    }
}

fn build_preset(p: Preset, ids: &[PaneId]) -> Node {
    if ids.len() == 1 {
        return Node::Leaf(ids[0]);
    }
    match p {
        Preset::EvenHorizontal => even(Axis::Horizontal, ids),
        Preset::EvenVertical => even(Axis::Vertical, ids),
        Preset::MainVertical => Node::Split {
            dir: Axis::Horizontal,
            ratio: 0.5,
            a: Box::new(Node::Leaf(ids[0])),
            b: Box::new(even(Axis::Vertical, &ids[1..])),
        },
        Preset::MainHorizontal => Node::Split {
            dir: Axis::Vertical,
            ratio: 0.5,
            a: Box::new(Node::Leaf(ids[0])),
            b: Box::new(even(Axis::Horizontal, &ids[1..])),
        },
        Preset::Tree => Node::Leaf(ids[0]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };

    fn cells(r: Rect) -> u32 {
        r.w as u32 * r.h as u32
    }

    fn layout(n: u32) -> Layout {
        let mut l = Layout::new(AREA);
        for id in 1..=n {
            l.insert(id, None, None);
        }
        l
    }

    /// 2x2 grid: 1 top-left, 2 top-right, 3 bottom-left, 4 bottom-right.
    fn grid() -> Layout {
        let mut l = Layout::new(AREA);
        l.insert(1, None, None);
        l.insert(2, Some(1), Some(Dir::Right));
        l.insert(3, Some(1), Some(Dir::Down));
        l.insert(4, Some(2), Some(Dir::Down));
        l
    }

    /// Assert the tiling covers `area` exactly: no gaps, no overlaps, all inside.
    fn assert_exact(l: &Layout) {
        let g = l.geometry();
        let total: u32 = g.iter().map(|(_, r)| cells(*r)).sum();
        assert_eq!(total, cells(l.area()), "coverage mismatch: {g:?}");
        for (i, (_, a)) in g.iter().enumerate() {
            assert!(
                a.x >= l.area().x
                    && a.y >= l.area().y
                    && a.right() <= l.area().right()
                    && a.bottom() <= l.area().bottom(),
                "{a:?} escapes area"
            );
            for (_, b) in g.iter().skip(i + 1) {
                assert!(!a.intersects(b), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn new_is_empty() {
        let l = Layout::new(AREA);
        assert!(l.is_empty());
        assert!(l.geometry().is_empty());
        assert_eq!(l.ids(), Vec::<PaneId>::new());
    }

    #[test]
    fn single_pane_fills_area() {
        let l = layout(1);
        assert_eq!(l.geometry(), vec![(1, AREA)]);
        assert_eq!(l.rect_of(1), Some(AREA));
        assert_exact(&l);
    }

    #[test]
    fn splits_tile_exactly() {
        // A handful of deterministic but varied split sequences.
        for seed in 0u32..6 {
            let mut l = Layout::new(Rect::new(2, 1, 77 + seed as u16, 23));
            l.insert(1, None, None);
            let mut placed = 1;
            for id in 2..=9u32 {
                let ids = l.ids();
                let near = ids[((id * 7 + seed) as usize) % ids.len()];
                let dir = match (id + seed) % 4 {
                    0 => Some(Dir::Right),
                    1 => Some(Dir::Down),
                    2 => Some(Dir::Left),
                    _ => None,
                };
                // 23 rows only halve so far: some of these splits are refused.
                placed += usize::from(l.insert(id, Some(near), dir));
                assert_exact(&l);
            }
            assert_eq!(l.ids().len(), placed);
        }
    }

    #[test]
    fn explicit_split_directions() {
        let mut l = layout(1);
        l.insert(2, Some(1), Some(Dir::Right));
        let (a, b) = (l.rect_of(1).unwrap(), l.rect_of(2).unwrap());
        assert_eq!(a.y, b.y);
        assert_eq!(a.right(), b.x);

        let mut l = layout(1);
        l.insert(2, Some(1), Some(Dir::Up));
        let (a, b) = (l.rect_of(1).unwrap(), l.rect_of(2).unwrap());
        // The new pane goes above.
        assert_eq!(b.bottom(), a.y);
        assert_eq!(l.ids(), vec![2, 1]);
    }

    #[test]
    fn default_split_follows_cell_aspect_not_the_longer_side() {
        // 80x24 is "wide" in cell aspect terms -> side by side.
        let mut l = layout(1);
        l.insert(2, None, None);
        assert_eq!(l.rect_of(1).unwrap().h, 24);
        // 20x24 is tall -> stacked.
        let mut l = Layout::new(Rect::new(0, 0, 20, 24));
        l.insert(1, None, None);
        l.insert(2, None, None);
        assert_eq!(l.rect_of(1).unwrap().w, 20);
        // 40x24 is wider than it is tall but not twice as wide, so cells being
        // roughly 1x2 make it the taller of the two on screen: stacked.
        let mut l = Layout::new(Rect::new(0, 0, 40, 24));
        l.insert(1, None, None);
        l.insert(2, None, None);
        assert_eq!(l.rect_of(1).unwrap().w, 40);
    }

    #[test]
    fn remove_collapses_sibling() {
        let mut l = grid();
        l.remove(2);
        assert_eq!(l.ids(), vec![1, 3, 4]);
        // 4 now owns the whole right column.
        assert_eq!(l.rect_of(4).unwrap().h, 24);
        assert_exact(&l);
    }

    #[test]
    fn remove_all_leaves_empty() {
        let mut l = grid();
        for id in [1, 2, 3, 4] {
            l.remove(id);
        }
        assert!(l.is_empty());
        assert!(l.geometry().is_empty());
    }

    #[test]
    fn remove_retiles_exactly() {
        let mut l = layout(6);
        l.remove(3);
        assert_exact(&l);
        l.remove(1);
        assert_exact(&l);
    }

    #[test]
    fn resize_is_reversible() {
        let mut l = grid();
        let before = l.rect_of(1).unwrap();
        l.resize(1, Dir::Right, 6);
        let after = l.rect_of(1).unwrap();
        assert_eq!(after.w, before.w + 6);
        assert_exact(&l);
        l.resize(1, Dir::Left, 6);
        assert_eq!(l.rect_of(1).unwrap(), before);
        assert_exact(&l);
    }

    #[test]
    fn resize_beyond_the_far_edge_stops_at_min() {
        let mut l = grid();
        l.resize(1, Dir::Right, 200);
        assert!(l.rect_of(2).unwrap().w >= MIN);
        assert_exact(&l);
        l.resize(1, Dir::Left, 200);
        assert!(l.rect_of(1).unwrap().w >= MIN);
        assert_exact(&l);
    }

    #[test]
    fn resize_switches_preset_to_tree() {
        let mut l = layout(3);
        l.set_preset(Preset::EvenHorizontal);
        l.resize(1, Dir::Right, 2);
        assert_eq!(l.preset, Preset::Tree);
    }

    #[test]
    fn preset_even_horizontal() {
        for n in [1u32, 2, 3, 5] {
            let mut l = layout(n);
            l.set_preset(Preset::EvenHorizontal);
            assert_exact(&l);
            let g = l.geometry();
            assert_eq!(g.len() as u32, n);
            for (_, r) in &g {
                assert_eq!(r.h, 24);
                assert!(r.w >= 80 / n as u16 - 1 && r.w <= 80 / n as u16 + 1);
            }
        }
    }

    #[test]
    fn preset_even_vertical() {
        for n in [1u32, 2, 3, 5] {
            let mut l = layout(n);
            l.set_preset(Preset::EvenVertical);
            assert_exact(&l);
            for (_, r) in l.geometry() {
                assert_eq!(r.w, 80);
            }
        }
    }

    #[test]
    fn preset_main_vertical() {
        let mut l = layout(5);
        l.set_preset(Preset::MainVertical);
        assert_exact(&l);
        let main = l.rect_of(1).unwrap();
        assert_eq!(main, Rect::new(0, 0, 40, 24));
        for id in 2..=5 {
            let r = l.rect_of(id).unwrap();
            assert_eq!(r.x, 40);
            assert_eq!(r.w, 40);
        }
    }

    #[test]
    fn preset_main_horizontal() {
        let mut l = layout(3);
        l.set_preset(Preset::MainHorizontal);
        assert_exact(&l);
        assert_eq!(l.rect_of(1).unwrap(), Rect::new(0, 0, 80, 12));
        assert_eq!(l.rect_of(2).unwrap().y, 12);
        assert_eq!(l.rect_of(3).unwrap().y, 12);
    }

    #[test]
    fn preset_preserves_order() {
        let mut l = grid();
        let before = l.ids();
        for p in [
            Preset::EvenHorizontal,
            Preset::EvenVertical,
            Preset::MainVertical,
            Preset::MainHorizontal,
        ] {
            l.set_preset(p);
            assert_eq!(l.ids(), before, "{p:?} reordered panes");
            assert_eq!(l.preset, p);
        }
    }

    #[test]
    fn set_area_preserves_ratios() {
        let mut l = grid();
        l.resize(1, Dir::Right, 10); // 50/30 split
        let frac = l.rect_of(1).unwrap().w as f32 / 80.0;
        l.set_area(Rect::new(0, 0, 160, 48));
        assert_exact(&l);
        let new_frac = l.rect_of(1).unwrap().w as f32 / 160.0;
        assert!((frac - new_frac).abs() < 0.02, "{frac} vs {new_frac}");
    }

    #[test]
    fn neighbor_in_grid() {
        let l = grid();
        assert_eq!(l.neighbor(1, Dir::Right), Some(2));
        assert_eq!(l.neighbor(1, Dir::Down), Some(3));
        assert_eq!(l.neighbor(2, Dir::Left), Some(1));
        assert_eq!(l.neighbor(4, Dir::Up), Some(2));
        assert_eq!(l.neighbor(3, Dir::Right), Some(4));
        assert_eq!(l.neighbor(4, Dir::Left), Some(3));
    }

    #[test]
    fn neighbor_none_at_edges() {
        let l = grid();
        assert_eq!(l.neighbor(1, Dir::Left), None);
        assert_eq!(l.neighbor(1, Dir::Up), None);
        assert_eq!(l.neighbor(4, Dir::Right), None);
        assert_eq!(l.neighbor(4, Dir::Down), None);
    }

    #[test]
    fn next_and_prev_wrap() {
        let l = grid();
        assert_eq!(l.next(1), Some(3));
        assert_eq!(l.prev(3), Some(1));
        assert_eq!(l.ids(), vec![1, 3, 2, 4]);
        assert_eq!(l.next(4), Some(1));
        assert_eq!(l.prev(1), Some(4));
        assert_eq!(l.next(99), None);
    }

    #[test]
    fn swap_next_exchanges_positions() {
        let mut l = grid();
        let (a, b) = (l.rect_of(1).unwrap(), l.rect_of(3).unwrap());
        l.swap_next(1);
        assert_eq!(l.rect_of(1), Some(b));
        assert_eq!(l.rect_of(3), Some(a));
        assert_exact(&l);
    }

    #[test]
    fn free_mode_seeds_from_tiling() {
        let mut l = grid();
        let before = l.geometry();
        l.set_mode(Mode::Free);
        let after = l.geometry();
        assert_eq!(before.len(), after.len());
        for (id, r) in before {
            assert_eq!(l.rect_of(id), Some(r), "pane {id} jumped");
        }
    }

    #[test]
    fn free_move_keeps_a_grabbable_sliver_on_screen() {
        let mut l = grid();
        l.set_mode(Mode::Free);
        // Left and top are hard edges: unsigned coordinates cannot go past them.
        l.move_pane(1, Dir::Left, 100);
        assert_eq!(l.rect_of(1).unwrap().x, AREA.x);
        l.move_pane(1, Dir::Up, 100);
        assert_eq!(l.rect_of(1).unwrap().y, AREA.y);

        // Right and bottom let the pane hang off, keeping MIN cells reachable.
        l.move_pane(1, Dir::Right, 500);
        assert_eq!(l.rect_of(1).unwrap().x, AREA.right() - MIN);
        l.move_pane(1, Dir::Down, 500);
        assert_eq!(l.rect_of(1).unwrap().y, AREA.bottom() - MIN);

        // And it can be dragged back out again.
        l.move_pane(1, Dir::Up, 500);
        assert_eq!(l.rect_of(1).unwrap().y, AREA.y);
    }

    #[test]
    fn overlapping_floats_hit_topmost() {
        let mut l = grid();
        l.set_mode(Mode::Free);
        l.move_pane(2, Dir::Left, 40); // 2 now sits on top of 1
        assert!(l.rect_of(1).unwrap().intersects(&l.rect_of(2).unwrap()));
        let p = l.rect_of(2).unwrap();
        assert_eq!(l.pane_at(p.x + 1, p.y + 1), Some(2));
        l.raise(1);
        assert_eq!(l.pane_at(p.x + 1, p.y + 1), Some(1));
    }

    #[test]
    fn raise_reorders_z_and_draw_order() {
        let mut l = grid();
        l.set_mode(Mode::Free);
        assert_eq!(l.geometry().last().unwrap().0, 4);
        l.raise(1);
        assert_eq!(l.geometry().last().unwrap().0, 1);
    }

    #[test]
    fn toggle_float_round_trip() {
        let mut l = grid();
        let before = l.rect_of(2).unwrap();
        l.toggle_float(2);
        assert!(l.is_floating(2));
        assert_eq!(l.rect_of(2), Some(before));
        assert_eq!(l.geometry().last().unwrap().0, 2);
        l.toggle_float(2);
        assert!(!l.is_floating(2));
        assert_exact(&l);
        assert_eq!(l.ids().len(), 4);
    }

    #[test]
    fn toggle_float_cascades_but_terminates_on_a_tiny_area() {
        // The cascade nudges a float that would land exactly on another one.
        // On a 3x3 area `clamp_rect` pins every nudge back to (0, 0), so this
        // used to spin forever and hang the UI.
        let mut l = Layout::new(Rect::new(0, 0, 3, 3));
        l.insert(1, None, None);
        l.toggle_float(1);
        l.insert(2, None, None);
        l.toggle_float(2);
        assert_eq!(l.ids().len(), 2);
        assert_eq!(l.rect_of(1), l.rect_of(2), "no room to cascade here");

        // Same sequence with room: the cascade still offsets the newcomer.
        let mut l = layout(1);
        l.toggle_float(1);
        l.insert(2, None, None);
        l.toggle_float(2);
        let (a, b) = (l.rect_of(1).unwrap(), l.rect_of(2).unwrap());
        assert_eq!((b.x, b.y), (a.x + 2, a.y + 1));
    }

    #[test]
    fn float_bookkeeping_never_outlives_a_pane() {
        // `desired` is what makes a resize round-trip exact, so it is written
        // from several places. A stale entry would resurrect a dead pane's
        // geometry under a recycled id.
        let mut l = layout(4);
        l.toggle_float(2);
        l.set_mode(Mode::Free);
        l.move_pane(3, Dir::Right, 5);
        l.swap_next(1);
        l.remove(3);
        l.remove(2);
        l.set_mode(Mode::Tiling);
        l.remove(1);
        let ids = l.ids();
        assert_eq!(ids, vec![4]);
        for map in [
            l.desired.keys().copied().collect::<Vec<_>>(),
            l.rects.keys().copied().collect(),
            l.z.clone(),
            l.explicit.clone(),
        ] {
            assert!(
                map.iter().all(|id| ids.contains(id)),
                "stale entry: {map:?}"
            );
        }
    }

    #[test]
    fn insert_refuses_when_there_is_no_room_to_split() {
        for w in [1u16, 2, 5] {
            let mut l = Layout::new(Rect::new(0, 0, w, 10));
            assert!(l.insert(1, None, None));
            assert!(!l.insert(2, Some(1), Some(Dir::Right)), "split at w={w}");
            assert_eq!(l.ids(), vec![1]);
            assert_eq!(l.rect_of(1).unwrap().w, w);
        }
        // 2 * MIN across is exactly enough, and both halves are usable.
        let mut l = Layout::new(Rect::new(0, 0, 6, 10));
        assert!(l.insert(1, None, None));
        assert!(l.insert(2, Some(1), Some(Dir::Right)));
        for (_, r) in l.geometry() {
            assert!(r.w >= MIN && r.h >= MIN, "unusable pane {r:?}");
        }
        assert_exact(&l);
    }

    #[test]
    fn toggle_float_refuses_to_dock_without_room() {
        let mut l = Layout::new(Rect::new(0, 0, 4, 4));
        l.insert(1, None, None);
        l.insert(2, None, None); // refused: no room
        l.toggle_float(1);
        l.toggle_float(1); // docks again: it is the only pane
        assert!(!l.is_floating(1));

        let mut l = Layout::new(Rect::new(0, 0, 5, 4));
        l.insert(1, None, None);
        l.toggle_float(1);
        l.insert(2, None, None);
        l.toggle_float(1); // would have to split a 5x4 pane: no room either way
        assert!(l.is_floating(1), "docked into an unusable tile");
    }

    #[test]
    fn hit_test_kinds_on_float() {
        let mut l = grid();
        l.toggle_float(2);
        let r = l.rect_of(2).unwrap();
        let sides = |left, right, top, bottom| {
            Some((
                2,
                DragKind::Resize {
                    left,
                    right,
                    top,
                    bottom,
                },
            ))
        };
        // Middle of the top row still moves; everything else on the rim resizes.
        assert_eq!(l.hit_test(r.x + 2, r.y), Some((2, DragKind::Move)));
        assert_eq!(l.hit_test(r.x, r.y + 2), sides(true, false, false, false));
        assert_eq!(
            l.hit_test(r.right() - 1, r.y + 2),
            sides(false, true, false, false)
        );
        assert_eq!(
            l.hit_test(r.x + 2, r.bottom() - 1),
            sides(false, false, false, true)
        );
        assert_eq!(l.hit_test(r.x + 2, r.y), Some((2, DragKind::Move)));
        // All four corners, the top two winning over the title row.
        assert_eq!(l.hit_test(r.x, r.y), sides(true, false, true, false));
        assert_eq!(
            l.hit_test(r.right() - 1, r.y),
            sides(false, true, true, false)
        );
        assert_eq!(
            l.hit_test(r.x, r.bottom() - 1),
            sides(true, false, false, true)
        );
        assert_eq!(
            l.hit_test(r.right() - 1, r.bottom() - 1),
            sides(false, true, false, true)
        );
        assert_eq!(l.hit_test(r.x + 2, r.y + 2), None);
    }

    #[test]
    fn resize_from_top_left_moves_the_origin() {
        let mut l = grid();
        l.toggle_float(2);
        let r = l.rect_of(2).unwrap();
        assert!(l.drag_start(r.x, r.y));
        l.drag_to(r.x + 4, r.y + 2);
        let moved = l.rect_of(2).unwrap();
        // The far corner is anchored: the pane shrinks by exactly the delta.
        assert_eq!((moved.x, moved.y), (r.x + 4, r.y + 2));
        assert_eq!((moved.right(), moved.bottom()), (r.right(), r.bottom()));
        // Back to the press point restores the rect.
        l.drag_to(r.x, r.y);
        assert_eq!(l.rect_of(2), Some(r));
    }

    #[test]
    fn resize_past_the_opposite_side_stops_at_min() {
        let mut l = grid();
        l.toggle_float(2);
        let r = l.rect_of(2).unwrap();
        // Left edge dragged far past the right edge.
        assert!(l.drag_start(r.x, r.y + 2));
        l.drag_to(r.right() + 30, r.y + 2);
        let got = l.rect_of(2).unwrap();
        assert_eq!(got.w, MIN, "inside out: {got:?}");
        assert_eq!(got.right(), r.right(), "right edge moved");
        l.drag_end();

        // Top-left corner dragged far past the bottom edge.
        let mut l = grid();
        l.toggle_float(2);
        let r = l.rect_of(2).unwrap();
        assert!(l.drag_start(r.x, r.y));
        l.drag_to(r.x, r.bottom() + 30);
        let got = l.rect_of(2).unwrap();
        assert_eq!(got.h, MIN, "inside out: {got:?}");
        assert_eq!(got.bottom(), r.bottom(), "bottom edge moved");
    }

    #[test]
    fn a_tiled_corner_moves_both_dividers_at_once() {
        // Two columns, the right one split in two rows, so the inner
        // horizontal divider meets the outer vertical one at a crossing.
        let mut l = Layout::new(Rect::new(0, 0, 80, 24));
        l.insert(1, None, None);
        l.insert(2, Some(1), Some(Dir::Right));
        l.insert(3, Some(2), Some(Dir::Down));

        let a = l.rect_of(1).unwrap();
        let b = l.rect_of(2).unwrap();
        let (cx, cy) = (a.right(), b.bottom());
        assert!(
            matches!(l.hit_test(cx, cy), Some((_, DragKind::Corner { .. }))),
            "no corner where the dividers cross: {:?}",
            l.hit_test(cx, cy)
        );

        assert!(l.drag_start(cx, cy));
        l.drag_to(cx - 10, cy - 4);
        l.drag_end();

        let a2 = l.rect_of(1).unwrap();
        let b2 = l.rect_of(2).unwrap();
        assert_eq!(a2.w, a.w - 10, "the vertical divider did not move");
        assert_eq!(b2.h, b.h - 4, "the horizontal divider did not move");
        // Still an exact tiling afterwards.
        assert_eq!(
            l.geometry()
                .iter()
                .map(|(_, r)| r.w as u32 * r.h as u32)
                .sum::<u32>(),
            80 * 24
        );
    }

    #[test]
    fn a_point_on_only_one_divider_is_still_a_plain_divider_drag() {
        let mut l = Layout::new(Rect::new(0, 0, 80, 24));
        l.insert(1, None, None);
        l.insert(2, Some(1), Some(Dir::Right));
        let x = l.rect_of(1).unwrap().right();
        assert!(matches!(l.hit_test(x, 10), Some((_, DragKind::Divider(_)))));
    }

    #[test]
    fn hit_test_finds_divider() {
        let l = grid();
        let boundary = l.rect_of(1).unwrap().right();
        match l.hit_test(boundary, 5) {
            Some((_, DragKind::Divider(i))) => assert_eq!(i, 0),
            other => panic!("expected the root divider, got {other:?}"),
        }
        assert!(matches!(
            l.hit_test(boundary - 1, 5),
            Some((_, DragKind::Divider(0)))
        ));
        assert_eq!(l.hit_test(10, 5), None);
    }

    #[test]
    fn divider_drag_keeps_exact_tiling() {
        let mut l = grid();
        let boundary = l.rect_of(1).unwrap().right();
        assert!(l.drag_start(boundary, 5));
        assert!(l.dragging());
        l.drag_to(boundary + 10, 5);
        assert_eq!(l.rect_of(1).unwrap().w, 50);
        assert_eq!(l.rect_of(2).unwrap().w, 30);
        assert_exact(&l);
        // Idempotent: the same pointer position gives the same result.
        l.drag_to(boundary + 10, 5);
        assert_eq!(l.rect_of(1).unwrap().w, 50);
        l.drag_to(0, 5);
        assert!(l.rect_of(1).unwrap().w >= MIN);
        assert_exact(&l);
        l.drag_end();
        assert!(!l.dragging());
    }

    #[test]
    fn divider_index_picks_the_nested_split_under_the_pointer() {
        // grid()'s splits in pre-order: 0 is the root column divider, 1 the
        // left column's row divider, 2 the right column's. Getting the index
        // wrong here drags the wrong divider, which is invisible in a layout
        // with only one split.
        let mut l = grid();
        assert_eq!(l.hit_test(60, 12), Some((2, DragKind::Divider(2))));
        assert_eq!(l.hit_test(20, 12), Some((1, DragKind::Divider(1))));

        assert!(l.drag_start(60, 12));
        l.drag_to(60, 18);
        assert_eq!(l.rect_of(2).unwrap().h, 18);
        assert_eq!(l.rect_of(4).unwrap().h, 6);
        assert_eq!(l.rect_of(1).unwrap().h, 12, "the left column moved too");
        assert_exact(&l);
    }

    #[test]
    fn resize_drag_on_a_float_thinner_than_min_does_not_panic() {
        // An area narrower than MIN forces the float below MIN, so the far
        // edge sits inside the minimum and the edge clamp has no room. It used
        // to build an inverted range and panic on the first drag event.
        let mut l = Layout::new(Rect::new(0, 0, 1, 8));
        l.insert(1, None, None);
        l.toggle_float(1);
        let r = l.rect_of(1).unwrap();
        assert_eq!(r.w, 1);
        assert!(l.drag_start(r.x, r.y + 2));
        l.drag_to(r.x + 4, r.y + 2);
        assert!(l.rect_of(1).unwrap().w >= 1);

        let mut l = Layout::new(Rect::new(0, 0, 8, 1));
        l.insert(1, None, None);
        l.toggle_float(1);
        assert!(l.drag_start(4, 0)); // the single row is both top and bottom
        l.drag_to(4, 6);
        assert!(l.rect_of(1).unwrap().h >= 1);
    }

    #[test]
    fn float_move_drag_keeps_grab_offset() {
        let mut l = grid();
        l.toggle_float(2);
        let r = l.rect_of(2).unwrap();
        assert!(l.drag_start(r.x + 5, r.y));
        l.drag_to(r.x - 5, r.y + 3);
        let moved = l.rect_of(2).unwrap();
        assert_eq!((moved.x, moved.y), (r.x - 10, r.y + 3));
        assert_eq!((moved.w, moved.h), (r.w, r.h));
        // Back to the press point restores the original rect.
        l.drag_to(r.x + 5, r.y);
        assert_eq!(l.rect_of(2), Some(r));
    }

    #[test]
    fn resize_drag_respects_minimum() {
        let mut l = grid();
        l.toggle_float(2);
        let r = l.rect_of(2).unwrap();
        assert!(l.drag_start(r.right() - 1, r.y + 2));
        l.drag_to(r.x, r.y + 2);
        assert_eq!(l.rect_of(2).unwrap().w, MIN);
        l.drag_to(r.x + 20, r.y + 2);
        assert_eq!(l.rect_of(2).unwrap().w, 21);
        assert!(l.rect_of(2).unwrap().right() <= AREA.right());
    }

    #[test]
    fn hit_test_finds_the_tile_title_grab() {
        let l = grid();
        let r = l.rect_of(2).unwrap(); // top-right tile
        assert_eq!(l.hit_test(r.x + 1, r.y), Some((2, DragKind::Grab)));
        assert_eq!(l.hit_test(r.x + GRAB, r.y), Some((2, DragKind::Grab)));
        // The left corner is the vertical divider, and the rest of the top
        // border past the title run stays free for the divider too.
        assert!(matches!(
            l.hit_test(r.x, r.y),
            Some((_, DragKind::Divider(_)))
        ));
        assert_eq!(l.hit_test(r.x + GRAB + 1, r.y), None);
        // Nothing to grab in the middle of a tile.
        assert_eq!(l.hit_test(r.x + 5, r.y + 5), None);
    }

    #[test]
    fn snap_target_picks_the_nearest_edge() {
        // Drag pane 4 (bottom-right) over pane 1 (top-left, 40x12).
        for (x, y, want) in [
            (1, 6, Dir::Left),
            (38, 6, Dir::Right),
            (20, 0, Dir::Up),
            (20, 11, Dir::Down),
        ] {
            let mut l = grid();
            let r = l.rect_of(4).unwrap();
            assert!(l.drag_start(r.x + 1, r.y), "no grab on pane 4");
            l.drag_to(x, y);
            let (target, dir, half) = l.snap_target().expect("a drop target");
            assert_eq!((target, dir), (1, want), "pointer at {x},{y}");
            let one = l.rect_of(1).unwrap();
            assert!(one.intersects(&half) && half.w <= one.w && half.h <= one.h);
        }

        // On itself, or on nothing, there is no target.
        let mut l = grid();
        let r = l.rect_of(4).unwrap();
        assert!(l.drag_start(r.x + 1, r.y));
        l.drag_to(r.x + 5, r.y + 5);
        assert_eq!(l.snap_target(), None);
        l.drag_to(AREA.right() + 5, 5);
        assert_eq!(l.snap_target(), None);
    }

    #[test]
    fn dropping_a_tile_on_a_half_reparents_it() {
        let mut l = grid();
        let r = l.rect_of(4).unwrap();
        assert!(l.drag_start(r.x + 1, r.y));
        l.drag_to(20, 11); // bottom half of pane 1
        let (_, _, half) = l.snap_target().unwrap();
        l.drag_end();

        // Pane 2 took the whole right column when 4 left; 1 and 4 now share
        // the left one, 4 underneath, exactly where the preview was.
        assert_eq!(l.rect_of(4), Some(half));
        assert_eq!(l.rect_of(1), Some(Rect::new(0, 0, 40, 6)));
        assert_eq!(l.rect_of(4), Some(Rect::new(0, 6, 40, 6)));
        assert_eq!(l.rect_of(2), Some(Rect::new(40, 0, 40, 24)));
        let mut ids = l.ids();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 3, 4], "a pane went missing");
        assert_exact(&l);
    }

    #[test]
    fn a_refused_drop_leaves_the_tree_untouched() {
        // Two panes side by side in an area only 5 rows tall: wide enough to
        // split again, nowhere near tall enough.
        let mut l = Layout::new(Rect::new(0, 0, 8, 5));
        l.insert(1, None, None);
        l.insert(2, Some(1), Some(Dir::Right));
        let before = l.geometry();

        let r = l.rect_of(2).unwrap();
        assert!(l.drag_start(r.x + 1, r.y));
        l.drag_to(2, 0); // top half of pane 1
        assert_eq!(l.snap_target().map(|(t, d, _)| (t, d)), Some((1, Dir::Up)));
        l.drag_end();

        assert_eq!(l.geometry(), before, "a refused drop moved things");
        assert_exact(&l);
    }

    #[test]
    fn zoom_fills_area() {
        let mut l = grid();
        l.set_zoom(Some(3));
        assert_eq!(l.geometry(), vec![(3, AREA)]);
        assert_eq!(l.pane_at(70, 2), Some(3));
        assert_eq!(l.rect_of(3), Some(AREA));
        assert_eq!(l.hit_test(40, 5), None);
        l.set_zoom(None);
        assert_eq!(l.pane_at(70, 2), Some(2));
        assert_exact(&l);
    }

    #[test]
    fn rect_edges_are_half_open_and_shrink_saturates() {
        let r = Rect::new(2, 3, 10, 5);
        assert_eq!((r.right(), r.bottom()), (12, 8));
        assert!(r.contains(2, 3) && r.contains(11, 7));
        assert!(!r.contains(12, 3) && !r.contains(2, 8));
        assert_eq!(r.shrink(1), Rect::new(3, 4, 8, 3));
        assert_eq!(r.shrink(99), Rect::new(101, 102, 0, 0));
        assert!(r.intersects(&Rect::new(11, 7, 4, 4)));
        assert!(!r.intersects(&Rect::new(12, 3, 4, 4)));
        let rt: ratatui::layout::Rect = r.into();
        assert_eq!(Rect::from(rt), r);
    }
}
