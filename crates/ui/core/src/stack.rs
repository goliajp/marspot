//! Columns and rows.
//!
//! Children are sized once with the main axis unbounded, then moved to
//! where they go.  A child is laid out a second time only when it is
//! handed an extent it did not ask for: a stretched child, a growing
//! one, or a scroll region that has to give way because the column no
//! longer fits.

use crate::geom::{Rect, Size};
use crate::layout::{Constraints, Pass};
use crate::node::{AlignCross, Distribute, Kind};

fn main_cross(vertical: bool, s: Size) -> (f64, f64) {
    if vertical { (s.h, s.w) } else { (s.w, s.h) }
}

/// What sizing the children found, before any of them is placed.
struct Sized {
    /// Children that take room: everything but rules.
    count: usize,
    /// Shares asked for by spacers and growing children.
    sum_flex: u32,
    /// Main-axis extent of the children that have their own size.
    intrinsic: f64,
    cross_used: f64,
    /// Scroll regions among them, which can be made shorter.
    scrolls: usize,
}

impl Pass<'_> {
    pub(crate) fn stack(&mut self, i: u32, origin: (f64, f64), c: Constraints, vertical: bool) -> Size {
        let tree = self.tree;
        let n = &tree.nodes[i as usize];
        let (align, distribute) = (n.align_cross, n.distribute);
        let gap = n.gap.resolve(0.0, self.m);
        let (cross_max, main_max) = if vertical { (c.max_w, c.max_h) } else { (c.max_h, c.max_w) };

        let mut sized = self.size_children(i, cross_max, vertical);
        let n_gaps = sized.count.saturating_sub(1) as f64;
        if vertical && sized.scrolls > 0 && sized.intrinsic + n_gaps * gap > main_max {
            let over = sized.intrinsic + n_gaps * gap - main_max;
            self.shorten_scrolls(i, cross_max, over / sized.scrolls as f64, &mut sized);
        }
        let used = sized.intrinsic + n_gaps * gap;
        let leftover = (main_max - used).max(0.0);
        let per_flex = if sized.sum_flex > 0 { leftover / sized.sum_flex as f64 } else { 0.0 };
        self.size_growers(i, cross_max, per_flex, vertical, &mut sized);

        let (lead, gap_extra) = if sized.sum_flex > 0 {
            (0.0, 0.0)
        } else {
            match distribute {
                Distribute::Start => (0.0, 0.0),
                Distribute::Center => (leftover * 0.5, 0.0),
                Distribute::End => (leftover, 0.0),
                Distribute::Spaced => {
                    let pad = leftover / (sized.count as f64 + 1.0);
                    (pad, pad)
                }
                Distribute::Between if sized.count <= 1 => (0.0, 0.0),
                Distribute::Between => (0.0, leftover / n_gaps),
            }
        };
        // a stack held wider than its children aligns them in the room it
        // was given, not in the room they would have taken
        let cross_min = if vertical { c.min_w } else { c.min_h };
        let cross_total =
            if align == AlignCross::Stretch { cross_max } else { sized.cross_used.max(cross_min) };

        let (mut cursor, mut edge, mut placed) = (lead, lead, 0usize);
        for ch in tree.children(i) {
            match tree.nodes[ch as usize].kind {
                Kind::Rule => {
                    self.place_rule(ch, origin, edge, cross_total, vertical);
                    continue;
                }
                Kind::Float => {
                    self.float(ch);
                    continue;
                }
                _ => {}
            }
            let child_main = self.place_child(
                ch, origin, cursor, per_flex, cross_max, cross_total, align, vertical,
            );
            cursor += child_main;
            edge = cursor;
            placed += 1;
            if placed < sized.count {
                cursor += gap + gap_extra;
            }
        }

        let total_main = if sized.sum_flex > 0 || distribute == Distribute::Spaced {
            main_max
        } else {
            used + lead
        };
        if vertical { c.clamp(cross_total, total_main) } else { c.clamp(total_main, cross_total) }
    }

    fn probe(cross_max: f64, vertical: bool) -> Constraints {
        if vertical {
            Constraints { min_w: 0.0, max_w: cross_max, min_h: 0.0, max_h: f64::INFINITY }
        } else {
            Constraints { min_w: 0.0, max_w: f64::INFINITY, min_h: 0.0, max_h: cross_max }
        }
    }

    fn size_children(&mut self, i: u32, cross_max: f64, vertical: bool) -> Sized {
        let tree = self.tree;
        let mut s = Sized { count: 0, sum_flex: 0, intrinsic: 0.0, cross_used: 0.0, scrolls: 0 };
        for ch in tree.children(i) {
            let cn = &tree.nodes[ch as usize];
            match cn.kind {
                Kind::Rule | Kind::Float => continue,
                Kind::Spacer => s.sum_flex += cn.flex as u32,
                _ if cn.style.grow > 0 => s.sum_flex += cn.style.grow as u32,
                kind => {
                    let size = self.node(ch, (0.0, 0.0), Self::probe(cross_max, vertical));
                    let (main, cross) = main_cross(vertical, size);
                    s.intrinsic += main;
                    s.cross_used = s.cross_used.max(cross);
                    s.scrolls += (kind == Kind::Scroll) as usize;
                }
            }
            s.count += 1;
        }
        s
    }

    /// The column is taller than its space: each scroll region in it
    /// gives up `cut` of its height and shows the rest by scrolling.
    fn shorten_scrolls(&mut self, i: u32, cross_max: f64, cut: f64, s: &mut Sized) {
        let tree = self.tree;
        for ch in tree.children(i) {
            let cn = &tree.nodes[ch as usize];
            if cn.kind != Kind::Scroll || cn.style.grow > 0 {
                continue;
            }
            let was = self.out.rects[ch as usize].h;
            let c = Constraints { max_h: (was - cut).max(0.0), ..Self::probe(cross_max, true) };
            s.intrinsic -= was - self.node(ch, (0.0, 0.0), c).h;
        }
    }

    /// A growing child is sized with its share before anything is
    /// placed, so its cross extent counts toward the stack's.
    fn size_growers(&mut self, i: u32, cross_max: f64, per_flex: f64, vertical: bool, s: &mut Sized) {
        let tree = self.tree;
        for ch in tree.children(i) {
            let cn = &tree.nodes[ch as usize];
            if cn.style.grow == 0 || matches!(cn.kind, Kind::Rule | Kind::Spacer | Kind::Float) {
                continue;
            }
            let main = per_flex * cn.style.grow as f64;
            let c = if vertical {
                Constraints { min_w: 0.0, max_w: cross_max, min_h: main, max_h: main }
            } else {
                Constraints { min_w: main, max_w: main, min_h: 0.0, max_h: cross_max }
            };
            let (_, cross) = main_cross(vertical, self.node(ch, (0.0, 0.0), c));
            s.cross_used = s.cross_used.max(cross);
        }
    }

    /// A rule goes on the edge the previous child ended at, across the
    /// stack, short of each side by its padding.
    fn place_rule(&mut self, ch: u32, origin: (f64, f64), edge: f64, cross_total: f64, vertical: bool) {
        let style = &self.tree.nodes[ch as usize].style;
        let m = self.m;
        let t = style.height.map_or(0.0, |l| l.resolve(0.0, m));
        let p = style.padding;
        let r = if vertical {
            let (a, b) = (p.left.resolve(cross_total, m), p.right.resolve(cross_total, m));
            Rect::new(origin.0 + a, origin.1 + edge, (cross_total - a - b).max(0.0), t)
        } else {
            let (a, b) = (p.top.resolve(cross_total, m), p.bottom.resolve(cross_total, m));
            Rect::new(origin.0 + edge, origin.1 + a, t, (cross_total - a - b).max(0.0))
        };
        self.out.rects[ch as usize] = r;
        self.out.inner[ch as usize] = r;
    }

    /// Put one sized child at `cursor` and return its main extent.
    fn place_child(
        &mut self,
        ch: u32,
        origin: (f64, f64),
        cursor: f64,
        per_flex: f64,
        cross_max: f64,
        cross_total: f64,
        align: AlignCross,
        vertical: bool,
    ) -> f64 {
        let cn = &self.tree.nodes[ch as usize];
        let spacer = cn.kind == Kind::Spacer;
        let stretching = align == AlignCross::Stretch;
        let (child_main, child_cross) = if spacer {
            (per_flex * cn.flex as f64, 0.0)
        } else {
            let r = self.out.rects[ch as usize];
            main_cross(vertical, Size { w: r.w, h: r.h })
        };
        let cross_at = match align {
            AlignCross::Start | AlignCross::Stretch => 0.0,
            AlignCross::Center => (cross_total - child_cross) * 0.5,
            AlignCross::End => cross_total - child_cross,
        };
        let at = if vertical {
            (origin.0 + cross_at, origin.1 + cursor)
        } else {
            (origin.0 + cursor, origin.1 + cross_at)
        };
        if spacer || stretching {
            let c = match (stretching, vertical) {
                (true, true) => Constraints::tight(cross_total, child_main),
                (true, false) => Constraints::tight(child_main, cross_total),
                (false, true) => {
                    Constraints { min_w: 0.0, max_w: cross_max, min_h: child_main, max_h: child_main }
                }
                (false, false) => {
                    Constraints { min_w: child_main, max_w: child_main, min_h: 0.0, max_h: cross_max }
                }
            };
            self.node(ch, at, c);
        } else {
            let r = self.out.rects[ch as usize];
            self.out.shift(self.tree, ch, at.0 - r.x, at.1 - r.y);
        }
        child_main
    }
}
