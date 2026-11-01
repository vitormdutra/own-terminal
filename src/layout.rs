use std::sync::atomic::Ordering;


use eframe::egui::{self, Pos2, Rect};

use crate::config::*;
use crate::pane::*;

/// Layout tree: leaves are terminals, splits hold two children and a draggable ratio.
pub enum Node {
    Empty,
    Leaf(Pane),
    Split { stacked: bool, ratio: f32, a: Box<Node>, b: Box<Node> },
}

/// Bookkeeping while drawing a tree.
pub struct Walk {
    pub idx: usize,
    pub focus: usize,
    pub new_focus: Option<usize>,
    pub div: usize,
    pub rects: Vec<Rect>,
}

pub const DIVIDER: f32 = 5.0;

impl Node {
    pub fn panes<'a>(&'a self, out: &mut Vec<&'a Pane>) {
        match self {
            Node::Empty => {}
            Node::Leaf(p) => out.push(p),
            Node::Split { a, b, .. } => { a.panes(out); b.panes(out); }
        }
    }

    pub fn panes_mut<'a>(&'a mut self, out: &mut Vec<&'a mut Pane>) {
        match self {
            Node::Empty => {}
            Node::Leaf(p) => out.push(p),
            Node::Split { a, b, .. } => { a.panes_mut(out); b.panes_mut(out); }
        }
    }

    pub fn count(&self) -> usize {
        let mut v = Vec::new();
        self.panes(&mut v);
        v.len()
    }

    /// Drops dead panes (a split with one dead child collapses into the other). True if nothing is left.
    pub fn prune(&mut self) -> bool {
        match self {
            Node::Empty => true,
            Node::Leaf(p) => p.dead.load(Ordering::Relaxed),
            Node::Split { a, b, .. } => {
                let (da, db) = (a.prune(), b.prune());
                if da && db { return true; }
                if da { *self = std::mem::replace(&mut **b, Node::Empty); }
                else if db { *self = std::mem::replace(&mut **a, Node::Empty); }
                false
            }
        }
    }

    /// Splits the `idx`-th leaf (DFS order), putting `new` after it.
    pub fn split(&mut self, idx: &mut usize, new: &mut Option<Pane>, stacked: bool) -> bool {
        match self {
            Node::Empty => false,
            Node::Leaf(_) if *idx == 0 => {
                let old = std::mem::replace(self, Node::Empty);
                let new = Node::Leaf(new.take().expect("pane"));
                *self = Node::Split { stacked, ratio: 0.5, a: Box::new(old), b: Box::new(new) };
                true
            }
            Node::Leaf(_) => { *idx -= 1; false }
            Node::Split { a, b, .. } => a.split(idx, new, stacked) || b.split(idx, new, stacked),
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, rect: Rect, v: &View, w: &mut Walk) {
        match self {
            Node::Empty => {}
            Node::Leaf(p) => {
                let k = w.idx;
                w.idx += 1;
                w.rects.push(rect);
                if p.show(ui, rect, v, k == w.focus, k) { w.new_focus = Some(k); }
            }
            Node::Split { stacked, ratio, a, b } => {
                let total = if *stacked { rect.height() } else { rect.width() };
                let at = (total * *ratio).round();
                let (ra, rb, div) = if *stacked {
                    let y = rect.top() + at;
                    (
                        Rect::from_min_max(rect.min, Pos2::new(rect.right(), y - DIVIDER / 2.0)),
                        Rect::from_min_max(Pos2::new(rect.left(), y + DIVIDER / 2.0), rect.max),
                        Rect::from_min_max(Pos2::new(rect.left(), y - DIVIDER / 2.0), Pos2::new(rect.right(), y + DIVIDER / 2.0)),
                    )
                } else {
                    let x = rect.left() + at;
                    (
                        Rect::from_min_max(rect.min, Pos2::new(x - DIVIDER / 2.0, rect.bottom())),
                        Rect::from_min_max(Pos2::new(x + DIVIDER / 2.0, rect.top()), rect.max),
                        Rect::from_min_max(Pos2::new(x - DIVIDER / 2.0, rect.top()), Pos2::new(x + DIVIDER / 2.0, rect.bottom())),
                    )
                };
                let r = ui.interact(div, ui.scope_id().with(("div", w.div)), egui::Sense::DRAG);
                w.div += 1;
                if r.hovered() || r.dragged() {
                    ui.ctx().set_cursor_icon(if *stacked { egui::CursorIcon::ResizeVertical } else { egui::CursorIcon::ResizeHorizontal });
                }
                if let (true, Some(p)) = (r.dragged(), r.interact_pointer_pos()) {
                    let pos = if *stacked { p.y - rect.top() } else { p.x - rect.left() };
                    *ratio = (pos / total).clamp(0.1, 0.9);
                }
                ui.painter_at(div).rect_filled(div, 0.0, cfg().bar);
                a.show(ui, ra, v, w);
                b.show(ui, rb, v, w);
            }
        }
    }
}

pub struct Tab {
    pub root: Node,
    pub focus: usize,
    /// Focused pane maximized (Cmd+Shift+Enter).
    pub zoom: bool,
    /// Stable identity, so dragging a tab keeps working while it changes position.
    pub id: u64,
}

impl Tab {
    pub fn new(pane: Pane) -> Tab {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        Tab { root: Node::Leaf(pane), focus: 0, zoom: false, id: NEXT.fetch_add(1, Ordering::Relaxed) }
    }
}

/// The pane next to `rects[from]` in direction (dx, dy): its center lies that way and it overlaps on the other axis.
pub fn nav(rects: &[Rect], from: usize, dx: i32, dy: i32) -> Option<usize> {
    let c = *rects.get(from)?;
    let dist = |r: &Rect| (r.center() - c.center()).length();
    rects
        .iter()
        .enumerate()
        .filter(|(j, r)| {
            let d = r.center() - c.center();
            *j != from
                && if dx != 0 {
                    d.x * dx as f32 > 0.0 && r.top() < c.bottom() && r.bottom() > c.top()
                } else {
                    d.y * dy as f32 > 0.0 && r.left() < c.right() && r.right() > c.left()
                }
        })
        .min_by(|a, b| dist(a.1).total_cmp(&dist(b.1)))
        .map(|(j, _)| j)
}

#[cfg(test)]
mod nav_tests {
    use super::*;

    #[test]
    fn nav_picks_the_neighbour_in_that_direction() {
        // [0 | 1] over [2]
        let r = |x0, y0, x1, y1| Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1));
        let rects = [r(0.0, 0.0, 50.0, 50.0), r(50.0, 0.0, 100.0, 50.0), r(0.0, 50.0, 100.0, 100.0)];
        assert_eq!(nav(&rects, 0, 1, 0), Some(1));
        assert_eq!(nav(&rects, 1, -1, 0), Some(0));
        assert_eq!(nav(&rects, 0, 0, 1), Some(2));
        assert_eq!(nav(&rects, 2, 0, -1), Some(0)); // nearest center wins over pane 1
        assert_eq!(nav(&rects, 0, -1, 0), None);
    }
}
