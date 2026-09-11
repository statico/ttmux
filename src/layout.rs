//! Pure geometry: the tiling tree, floating rects, mouse hit-testing.
//!
//! No I/O and no terminal here — everything is computed from an `area` plus a
//! binary split tree (ratio-based, so resizes survive terminal resizes) and a
//! list of absolutely-positioned floating panes.

use std::collections::HashMap;

use crate::action::Dir;

/// Stable identifier for a pane. Allocated by the app, opaque here.
pub type PaneId = u32;

/// Smallest pane we ever produce, borders included.
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

/// What a mouse press grabbed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragKind {
    /// Move a floating pane.
    Move,
    /// Resize a floating pane by its right (`Dir::Right`) or bottom edge.
    ResizeEdge(Dir),
    /// Move the divider of the n-th split node (pre-order index).
    Divider(usize),
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
    /// Rect of the float at press time (unused for dividers).
    start: Rect,
    /// Bottom-right corner grab: resize both axes.
    corner: bool,
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
            explicit: Vec::new(),
            z: Vec::new(),
            drag: None,
        }
    }

    /// Resize the whole layout. Tiled ratios are kept; float rects scale with it.
    pub fn set_area(&mut self, area: Rect) {
        let old = self.area;
        self.area = area;
        if old.w > 0 && old.h > 0 {
            let ids: Vec<PaneId> = self.rects.keys().copied().collect();
            for id in ids {
                let r = self.rects[&id];
                let scaled = Rect::new(
                    area.x + scale(r.x.saturating_sub(old.x), old.w, area.w),
                    area.y + scale(r.y.saturating_sub(old.y), old.h, area.h),
                    scale(r.w, old.w, area.w).max(1),
                    scale(r.h, old.h, area.h).max(1),
                );
                let clamped = self.clamp_rect(scaled);
                self.rects.insert(id, clamped);
            }
        }
    }

    pub fn area(&self) -> Rect {
        self.area
    }

    // ---------------------------------------------------------------- panes

    /// Add a pane, splitting `near` (or the last pane) along `dir`.
    ///
    /// `dir` of `None` splits the longer side of that pane.
    pub fn insert(&mut self, id: PaneId, near: Option<PaneId>, dir: Option<Dir>) {
        if self.ids().contains(&id) {
            return;
        }
        let leaves = self.leaves();
        let near = near.filter(|n| leaves.contains(n)).or_else(|| leaves.last().copied());
        match (self.root.take(), near) {
            (None, _) | (_, None) => {
                // First tiled pane (or nothing to split): it becomes the whole tree.
                self.root = Some(Node::Leaf(id));
            }
            (Some(root), Some(near)) => {
                let rect = self.tiled_of(&root, near).unwrap_or(self.area);
                let axis = match dir {
                    Some(d) => Axis::of(d),
                    None => {
                        if rect.w / 2 >= rect.h {
                            Axis::Horizontal
                        } else {
                            Axis::Vertical
                        }
                    }
                };
                let first = matches!(dir, Some(Dir::Left) | Some(Dir::Up));
                let mut root = root;
                split_leaf(&mut root, near, id, axis, first);
                self.root = Some(root);
                self.preset = Preset::Tree;
            }
        }
        if self.mode == Mode::Free {
            // Give the newcomer the rect it would have had while tiled.
            if let Some(r) = self.tiled_geometry().into_iter().find(|(p, _)| *p == id) {
                self.rects.insert(id, r.1);
            } else {
                self.rects.insert(id, self.area.shrink(2));
            }
            self.z.push(id);
        }
    }

    /// Remove a pane; its sibling collapses into its place.
    pub fn remove(&mut self, id: PaneId) {
        if let Some(root) = self.root.take() {
            self.root = remove_leaf(root, id);
        }
        self.rects.remove(&id);
        self.explicit.retain(|p| *p != id);
        self.z.retain(|p| *p != id);
        if self.zoomed == Some(id) {
            self.zoomed = None;
        }
        if self.drag.map(|d| d.id) == Some(id) {
            self.drag = None;
        }
    }

    /// All panes: tiled ones in tree order, then floats back to front.
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

    /// Grow the pane by `n` cells towards `dir`.
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
            let grown = self.clamp_rect(grown);
            self.rects.insert(id, grown);
            return;
        }
        let axis = Axis::of(dir);
        let forward = matches!(dir, Dir::Right | Dir::Down);
        let Some(path) = self.leaf_path(id) else { return };
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
            let len = if axis == Axis::Horizontal { rect.w } else { rect.h };
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
        let Some(r) = self.rects.get(&id).copied() else { return };
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
        let moved = self.clamp_rect(moved);
        self.rects.insert(id, moved);
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
                    self.rects.insert(id, r);
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
            }
        }
        self.mode = m;
    }

    /// Float a tiled pane on top, or dock a floating one back into the tree.
    pub fn toggle_float(&mut self, id: PaneId) {
        if self.explicit.contains(&id) {
            self.explicit.retain(|p| *p != id);
            let leaves = self.leaves();
            match leaves.last().copied() {
                None => self.root = Some(Node::Leaf(id)),
                Some(near) => {
                    let rect = self.raw_rect(near).unwrap_or(self.area);
                    let axis = if rect.w / 2 >= rect.h {
                        Axis::Horizontal
                    } else {
                        Axis::Vertical
                    };
                    if let Some(root) = self.root.as_mut() {
                        split_leaf(root, near, id, axis, false);
                    }
                }
            }
            if self.mode == Mode::Tiling {
                self.z.retain(|p| *p != id);
                self.rects.remove(&id);
            }
            self.preset = Preset::Tree;
            return;
        }
        let Some(mut rect) = self.raw_rect(id) else { return };
        if let Some(root) = self.root.take() {
            self.root = remove_leaf(root, id);
        }
        // Don't land exactly on top of another float.
        while self
            .rects
            .iter()
            .any(|(p, r)| *p != id && *r == rect && self.z.contains(p))
        {
            rect = self.clamp_rect(Rect::new(rect.x + 2, rect.y + 1, rect.w, rect.h));
        }
        self.rects.insert(id, rect);
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
        self.zoomed = match id {
            Some(id) if self.ids().contains(&id) => Some(id),
            Some(_) => None,
            None => None,
        };
    }

    // ----------------------------------------------------------------- mice

    /// What a press at `(x, y)` would grab.
    pub fn hit_test(&self, x: u16, y: u16) -> Option<(PaneId, DragKind)> {
        if self.zoomed.is_some() {
            return None;
        }
        for id in self.z.iter().rev() {
            let Some(r) = self.rects.get(id) else { continue };
            if !r.contains(x, y) {
                continue;
            }
            let kind = if x + 1 == r.right() {
                Some(DragKind::ResizeEdge(Dir::Right))
            } else if y + 1 == r.bottom() {
                Some(DragKind::ResizeEdge(Dir::Down))
            } else if y == r.y {
                Some(DragKind::Move)
            } else {
                None
            };
            return kind.map(|k| (*id, k));
        }
        if self.mode == Mode::Tiling {
            if let Some(root) = self.root.as_ref() {
                let mut idx = 0;
                return hit_divider(root, self.area, x, y, &mut idx);
            }
        }
        None
    }

    /// Begin a drag. Returns false if nothing draggable is under the pointer.
    pub fn drag_start(&mut self, x: u16, y: u16) -> bool {
        let Some((id, kind)) = self.hit_test(x, y) else {
            self.drag = None;
            return false;
        };
        let start = self.rects.get(&id).copied().unwrap_or_default();
        let (grab, corner) = match kind {
            DragKind::Move => ((x as i32 - start.x as i32, y as i32 - start.y as i32), false),
            DragKind::ResizeEdge(_) => (
                (
                    x as i32 - start.right() as i32 + 1,
                    y as i32 - start.bottom() as i32 + 1,
                ),
                x + 1 == start.right() && y + 1 == start.bottom(),
            ),
            DragKind::Divider(i) => {
                let (rect, axis, _) = self.splits()[i];
                let len = if axis == Axis::Horizontal { rect.w } else { rect.h };
                let first = part(len, self.nth_ratio(i));
                let boundary = if axis == Axis::Horizontal {
                    rect.x + first
                } else {
                    rect.y + first
                };
                let g = if axis == Axis::Horizontal {
                    (x as i32 - boundary as i32, 0)
                } else {
                    (0, y as i32 - boundary as i32)
                };
                (g, false)
            }
        };
        if matches!(kind, DragKind::Move | DragKind::ResizeEdge(_)) {
            self.raise(id);
        }
        self.drag = Some(Drag {
            id,
            kind,
            grab,
            start,
            corner,
        });
        true
    }

    /// Continue a drag. Absolute coordinates; idempotent w.r.t. the press point.
    pub fn drag_to(&mut self, x: u16, y: u16) {
        let Some(d) = self.drag else { return };
        match d.kind {
            DragKind::Move => {
                let nx = (x as i32 - d.grab.0).max(0) as u16;
                let ny = (y as i32 - d.grab.1).max(0) as u16;
                let r = self.clamp_rect(Rect::new(nx, ny, d.start.w, d.start.h));
                self.rects.insert(d.id, r);
            }
            DragKind::ResizeEdge(dir) => {
                let mut r = d.start;
                if d.corner || dir == Dir::Right {
                    let right = (x as i32 - d.grab.0 + 1).max(0) as u16;
                    r.w = right.saturating_sub(r.x).max(MIN);
                }
                if d.corner || dir == Dir::Down {
                    let bottom = (y as i32 - d.grab.1 + 1).max(0) as u16;
                    r.h = bottom.saturating_sub(r.y).max(MIN);
                }
                let r = self.clamp_rect(r);
                self.rects.insert(d.id, r);
            }
            DragKind::Divider(i) => {
                let splits = self.splits();
                let Some(&(rect, axis, ref path)) = splits.get(i) else { return };
                let path = path.clone();
                let len = if axis == Axis::Horizontal { rect.w } else { rect.h };
                if len < 2 * MIN {
                    return;
                }
                let boundary = if axis == Axis::Horizontal {
                    x as i32 - d.grab.0 - rect.x as i32
                } else {
                    y as i32 - d.grab.1 - rect.y as i32
                };
                let first = boundary.clamp(MIN as i32, (len - MIN) as i32) as u16;
                self.set_ratio_at(&path, first as f32 / len as f32);
                self.preset = Preset::Tree;
            }
        }
    }

    pub fn drag_end(&mut self) {
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

    fn tiled_of(&self, root: &Node, id: PaneId) -> Option<Rect> {
        let mut out = Vec::new();
        collect_geometry(root, self.area, &mut out);
        out.into_iter().find(|(p, _)| *p == id).map(|(_, r)| r)
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

    fn nth_ratio(&self, i: usize) -> f32 {
        let splits = self.splits();
        splits.get(i).map_or(0.5, |(_, _, p)| self.ratio_at(p))
    }

    fn ratio_at(&self, path: &[bool]) -> f32 {
        let mut node = self.root.as_ref();
        for &b in path {
            match node {
                Some(Node::Split { a, b: bb, .. }) => {
                    node = Some(if b { bb } else { a });
                }
                _ => return 0.5,
            }
        }
        match node {
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

    /// Keep a rect inside `area`, at least MIN x MIN (or the whole area if smaller).
    fn clamp_rect(&self, mut r: Rect) -> Rect {
        let a = self.area;
        r.w = r.w.clamp(MIN.min(a.w).max(1), a.w.max(1));
        r.h = r.h.clamp(MIN.min(a.h).max(1), a.h.max(1));
        r.x = r.x.max(a.x).min(a.right().saturating_sub(r.w).max(a.x));
        r.y = r.y.max(a.y).min(a.bottom().saturating_sub(r.h).max(a.y));
        r
    }
}

// ------------------------------------------------------------ free functions

fn overlap(a0: u16, a1: u16, b0: u16, b1: u16) -> u16 {
    a1.min(b1).saturating_sub(a0.max(b0))
}

fn scale(v: u16, from: u16, to: u16) -> u16 {
    if from == 0 {
        return v;
    }
    ((v as u32 * to as u32) / from as u32) as u16
}

/// Size of the first child, with the remainder going to the second.
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

fn hit_divider(
    node: &Node,
    rect: Rect,
    x: u16,
    y: u16,
    idx: &mut usize,
) -> Option<(PaneId, DragKind)> {
    let Node::Split { dir, ratio, a, b } = node else {
        return None;
    };
    let here = *idx;
    *idx += 1;
    let (ra, rb) = split_rect(rect, *dir, *ratio);
    let on_edge = match dir {
        Axis::Horizontal => {
            rect.contains(x, y) && (x + 1 == ra.right() || x == rb.x) && ra.w > 0 && rb.w > 0
        }
        Axis::Vertical => {
            rect.contains(x, y) && (y + 1 == ra.bottom() || y == rb.y) && ra.h > 0 && rb.h > 0
        }
    };
    if on_edge {
        let mut leaves = Vec::new();
        collect_leaves(a, &mut leaves);
        return leaves.first().map(|id| (*id, DragKind::Divider(here)));
    }
    if ra.contains(x, y) {
        let r = hit_divider(a, ra, x, y, idx);
        // Keep the index counter consistent even when the hit is on the a side.
        let mut skip = *idx;
        count_splits(b, &mut skip);
        *idx = skip;
        return r;
    }
    let mut skip = *idx;
    count_splits(a, &mut skip);
    *idx = skip;
    hit_divider(b, rb, x, y, idx)
}

fn count_splits(node: &Node, n: &mut usize) {
    if let Node::Split { a, b, .. } = node {
        *n += 1;
        count_splits(a, n);
        count_splits(b, n);
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
            for id in 2..=9u32 {
                let ids = l.ids();
                let near = ids[((id * 7 + seed) as usize) % ids.len()];
                let dir = match (id + seed) % 4 {
                    0 => Some(Dir::Right),
                    1 => Some(Dir::Down),
                    2 => Some(Dir::Left),
                    _ => None,
                };
                l.insert(id, Some(near), dir);
                assert_exact(&l);
            }
            assert_eq!(l.ids().len(), 9);
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
    fn default_split_picks_longer_side() {
        // 80x24 is "wide" in cell aspect terms -> side by side.
        let mut l = layout(1);
        l.insert(2, None, None);
        assert_eq!(l.rect_of(1).unwrap().h, 24);
        // 20x24 is tall -> stacked.
        let mut l = Layout::new(Rect::new(0, 0, 20, 24));
        l.insert(1, None, None);
        l.insert(2, None, None);
        assert_eq!(l.rect_of(1).unwrap().w, 20);
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
    fn resize_respects_minimum() {
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
    fn free_move_clamps_to_area() {
        let mut l = grid();
        l.set_mode(Mode::Free);
        l.move_pane(1, Dir::Left, 100);
        assert_eq!(l.rect_of(1).unwrap().x, AREA.x);
        l.move_pane(1, Dir::Right, 500);
        let r = l.rect_of(1).unwrap();
        assert_eq!(r.right(), AREA.right());
        l.move_pane(1, Dir::Down, 500);
        assert_eq!(l.rect_of(1).unwrap().bottom(), AREA.bottom());
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
    fn hit_test_kinds_on_float() {
        let mut l = grid();
        l.toggle_float(2);
        let r = l.rect_of(2).unwrap();
        assert_eq!(l.hit_test(r.x + 2, r.y), Some((2, DragKind::Move)));
        assert_eq!(
            l.hit_test(r.right() - 1, r.y + 2),
            Some((2, DragKind::ResizeEdge(Dir::Right)))
        );
        assert_eq!(
            l.hit_test(r.x + 2, r.bottom() - 1),
            Some((2, DragKind::ResizeEdge(Dir::Down)))
        );
        assert_eq!(l.hit_test(r.x + 2, r.y + 2), None);
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
    fn rect_helpers() {
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
