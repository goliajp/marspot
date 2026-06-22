//! Layout pass — `View` tree → `LaidOut` tree.
//!
//! Two-pass constraints algorithm, parent-down + child-up.  See
//! `docs/ui-system-model.md` §6 for the canonical algorithm.
//!
//! All values here are physical pixels.  Logical-pt → phys
//! resolution happens at `Length::resolve_*` boundaries; once a
//! number enters this module it's phys.

use crate::ui::core::Length;
use super::types::{AlignCross, Anchor, Distribute, Edges, FrameSpec};
use super::view::{Modifier, View, TextSize};

/// Two-axis (min, max) constraints handed down to a node.
/// Equivalent to Flutter's `BoxConstraints`.
#[derive(Clone, Copy, Debug)]
pub struct Constraints {
    pub min_w: f64,
    pub max_w: f64,
    pub min_h: f64,
    pub max_h: f64,
}

impl Constraints {
    pub fn loose(max_w: f64, max_h: f64) -> Self {
        Self { min_w: 0.0, max_w, min_h: 0.0, max_h }
    }

    pub fn tight(w: f64, h: f64) -> Self {
        Self { min_w: w, max_w: w, min_h: h, max_h: h }
    }

    /// Clamp a candidate size against these constraints.
    pub fn clamp(&self, w: f64, h: f64) -> Size {
        Size {
            w: w.max(self.min_w).min(self.max_w).max(0.0),
            h: h.max(self.min_h).min(self.max_h).max(0.0),
        }
    }

    /// Subtract edges from the max (for Padding modifier).  Min
    /// shrinks too but never below 0.
    pub fn shrink_by_edges(&self, e: &EdgesPhys) -> Self {
        let dx = e.left + e.right;
        let dy = e.top + e.bottom;
        Self {
            min_w: (self.min_w - dx).max(0.0),
            max_w: (self.max_w - dx).max(0.0),
            min_h: (self.min_h - dy).max(0.0),
            max_h: (self.max_h - dy).max(0.0),
        }
    }
}

/// Resolved size of a node in physical pixels.
#[derive(Clone, Copy, Debug, Default)]
pub struct Size {
    pub w: f64,
    pub h: f64,
}

/// A laid-out subtree — every node knows its final rect (phys).
/// `paint` and `hit_test` walk this tree.
#[derive(Clone, Debug)]
pub struct LaidOut {
    pub view: View,
    pub rect: Rect,
    /// Resolved modifier metadata (paint side reads this without
    /// re-walking the modifier chain).  Pre-baked at layout time.
    pub deco: Decoration,
    /// Children in submission order — for ZStack last is on top.
    pub children: Vec<LaidOut>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn contains(&self, p: (f64, f64)) -> bool {
        p.0 >= self.x && p.0 < self.x + self.w
            && p.1 >= self.y && p.1 < self.y + self.h
    }
}

/// Decoration baked from the modifier chain.  Paint reads this
/// directly instead of re-walking modifiers.
#[derive(Clone, Debug, Default)]
pub struct Decoration {
    pub bg: Option<crate::ui::core::Color>,
    pub border: Option<(f64, crate::ui::core::Color)>, // (width_phys, color)
    pub radius: f64, // phys
    pub shadow: Option<DecoShadow>,
    pub padding: EdgesPhys, // phys
    pub hidden: bool,
    pub on_click: Option<super::types::ActionId>,
    pub on_hover: Option<super::types::HoverId>,
    pub id: Option<super::types::ViewId>,
}

#[derive(Clone, Copy, Debug)]
pub struct DecoShadow {
    pub blur: f64,
    pub offset: (f64, f64),
    pub color: crate::ui::core::Color,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EdgesPhys {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

/// Resolution context — everything `Length::resolve_*` needs.
#[derive(Clone, Copy, Debug)]
pub struct LayoutCtx {
    /// Device pixel ratio (NSWindow backingScaleFactor).
    pub scale: f64,
    /// Chrome font cell metrics in physical pixels.
    pub cell_w_phys: f64,
    pub cell_h_phys: f64,
    /// Chrome font ascent — used for text baseline placement.
    pub ascent_phys: f64,
}

impl LayoutCtx {
    pub fn line_h_phys(&self, size: TextSize) -> f64 {
        let mult = match size {
            TextSize::Caption     => 0.85,
            TextSize::Body        => 1.00,
            TextSize::Header      => 1.20,
            TextSize::LargeHeader => 1.50,
        };
        self.cell_h_phys * mult
    }
}

// ─── Layout entry point ───────────────────────────────────────

/// Run the layout pass.  `origin` is the parent's top-left in phys;
/// `constraints` bounds this node's size.  Returns the laid-out
/// subtree with all rects in phys.
pub fn layout(view: &View, ctx: LayoutCtx, origin: (f64, f64), c: Constraints) -> LaidOut {
    match view {
        View::Text(t) => {
            // Width = char count × cell_w (clamped); height = lines × line_h.
            // Wrap currently truncated to single-line — wrap support lands
            // when we wire a real text shaper (out of v1 scope).
            let line_h = ctx.line_h_phys(t.size);
            let raw_w = t.content.chars().count() as f64 * ctx.cell_w_phys;
            let size = c.clamp(raw_w, line_h);
            LaidOut {
                view: view.clone(),
                rect: Rect { x: origin.0, y: origin.1, w: size.w, h: size.h },
                deco: Decoration::default(),
                children: Vec::new(),
            }
        }
        View::Spacer { .. } => {
            // Spacer on its own (outside a stack) collapses to min.
            // Inside a stack the parent decides how to expand it.
            let size = c.clamp(c.min_w, c.min_h);
            LaidOut {
                view: view.clone(),
                rect: Rect { x: origin.0, y: origin.1, w: size.w, h: size.h },
                deco: Decoration::default(),
                children: Vec::new(),
            }
        }
        View::Filled { .. } | View::Hairline { .. } => {
            // Both fill the constraints; Hairline doesn't enforce
            // axis-1 size here — caller should `.frame(height: Pt(1))`
            // or use a stack gap.
            let size = c.clamp(c.max_w, c.max_h);
            LaidOut {
                view: view.clone(),
                rect: Rect { x: origin.0, y: origin.1, w: size.w, h: size.h },
                deco: Decoration::default(),
                children: Vec::new(),
            }
        }
        View::VStack { children, gap, align, distribute } => {
            layout_stack(children, *gap, *align, *distribute, true, ctx, origin, c, view)
        }
        View::HStack { children, gap, align, distribute } => {
            layout_stack(children, *gap, *align, *distribute, false, ctx, origin, c, view)
        }
        View::ZStack { children, align } => {
            layout_zstack(children, *align, ctx, origin, c, view)
        }
        View::Modified { child, mods } => layout_modified(child, mods, ctx, origin, c, view),
    }
}

// ─── VStack / HStack ──────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn layout_stack(
    children: &[View],
    gap: Length,
    align: AlignCross,
    distribute: Distribute,
    vertical: bool,
    ctx: LayoutCtx,
    origin: (f64, f64),
    c: Constraints,
    self_view: &View,
) -> LaidOut {
    let gap_phys = gap.resolve_for_axis_with_cell(0.0, ctx.scale, ctx.cell_w_phys);
    // ── Pass A: intrinsic sizing for non-spacers + count spacer flex ──
    // For each non-spacer, give cross unconstrained-up-to-parent and
    // main unconstrained.  We need its preferred main extent.
    let cross_max = if vertical { c.max_w } else { c.max_h };
    let main_max  = if vertical { c.max_h } else { c.max_w };

    let mut sum_flex: u32 = 0;
    let mut intrinsic_main: f64 = 0.0;
    let mut cross_used: f64 = 0.0;
    let mut child_sizes: Vec<Option<Size>> = Vec::with_capacity(children.len());
    for ch in children {
        if let View::Spacer { flex } = ch {
            sum_flex += *flex;
            child_sizes.push(None);
            continue;
        }
        let inner_c = if vertical {
            Constraints { min_w: 0.0, max_w: cross_max, min_h: 0.0, max_h: f64::INFINITY }
        } else {
            Constraints { min_w: 0.0, max_w: f64::INFINITY, min_h: 0.0, max_h: cross_max }
        };
        let laid = layout(ch, ctx, (0.0, 0.0), inner_c);
        let s = Size { w: laid.rect.w, h: laid.rect.h };
        intrinsic_main += if vertical { s.h } else { s.w };
        cross_used = cross_used.max(if vertical { s.w } else { s.h });
        child_sizes.push(Some(s));
    }

    let n_gaps = (children.len().saturating_sub(1)) as f64;
    let used = intrinsic_main + n_gaps * gap_phys;
    let leftover = (main_max - used).max(0.0);
    let per_flex = if sum_flex > 0 { leftover / sum_flex as f64 } else { 0.0 };

    // ── Pass B: position children along main axis ──
    // Distribute logic.  If there are flex spacers, leftover is fully
    // consumed by them (start-style packing).  Otherwise distribute
    // applies to the leftover gap.
    let (lead_pad, gap_extra) = if sum_flex > 0 {
        (0.0, 0.0)
    } else {
        match distribute {
            Distribute::Start   => (0.0, 0.0),
            Distribute::Center  => (leftover * 0.5, 0.0),
            Distribute::End     => (leftover, 0.0),
            Distribute::Spaced  => {
                // space-around: gap before / between / after
                let slots = (children.len() as f64) + 1.0;
                let pad = leftover / slots;
                (pad, pad)
            }
            Distribute::Between => {
                if children.len() <= 1 {
                    (0.0, 0.0)
                } else {
                    (0.0, leftover / n_gaps)
                }
            }
        }
    };

    // Cross-axis size = either parent's max (Stretch) or content max.
    let cross_total = if matches!(align, AlignCross::Stretch) {
        cross_max.min(if vertical { c.max_w } else { c.max_h })
    } else {
        cross_used
    };

    let mut laid_children: Vec<LaidOut> = Vec::with_capacity(children.len());
    let mut cursor_main = lead_pad;
    for (i, ch) in children.iter().enumerate() {
        let (child_main, child_cross) = match (ch, child_sizes[i]) {
            (View::Spacer { flex }, _) => {
                let m = per_flex * (*flex as f64);
                (m, 0.0)
            }
            (_, Some(s)) => (
                if vertical { s.h } else { s.w },
                if vertical { s.w } else { s.h },
            ),
            _ => (0.0, 0.0),
        };

        // Cross-axis position depends on align.
        let cross_offset = if matches!(align, AlignCross::Stretch) {
            0.0
        } else {
            match align {
                AlignCross::Start   => 0.0,
                AlignCross::Center  => (cross_total - child_cross) * 0.5,
                AlignCross::End     => cross_total - child_cross,
                AlignCross::Stretch => 0.0,
            }
        };

        let child_origin = if vertical {
            (origin.0 + cross_offset, origin.1 + cursor_main)
        } else {
            (origin.0 + cursor_main, origin.1 + cross_offset)
        };

        let child_c = if matches!(align, AlignCross::Stretch) {
            if vertical {
                Constraints::tight(cross_total, child_main)
            } else {
                Constraints::tight(child_main, cross_total)
            }
        } else if vertical {
            Constraints {
                min_w: 0.0, max_w: cross_max,
                min_h: child_main, max_h: child_main,
            }
        } else {
            Constraints {
                min_w: child_main, max_w: child_main,
                min_h: 0.0, max_h: cross_max,
            }
        };

        let laid = layout(ch, ctx, child_origin, child_c);
        laid_children.push(laid);

        cursor_main += child_main;
        if i + 1 < children.len() {
            cursor_main += gap_phys + gap_extra;
        }
    }

    let total_main = if sum_flex > 0 || matches!(distribute, Distribute::Spaced) {
        main_max
    } else {
        used + lead_pad
    };
    let (w_self, h_self) = if vertical {
        (cross_total, total_main)
    } else {
        (total_main, cross_total)
    };
    let size = c.clamp(w_self, h_self);

    LaidOut {
        view: self_view.clone(),
        rect: Rect { x: origin.0, y: origin.1, w: size.w, h: size.h },
        deco: Decoration::default(),
        children: laid_children,
    }
}

// ─── ZStack ───────────────────────────────────────────────────

fn layout_zstack(
    children: &[View],
    align: Anchor,
    ctx: LayoutCtx,
    origin: (f64, f64),
    c: Constraints,
    self_view: &View,
) -> LaidOut {
    // Pass 1: lay each child against the full constraints; collect
    // their preferred sizes.
    let mut child_laid: Vec<LaidOut> = children.iter()
        .map(|ch| layout(ch, ctx, origin, c))
        .collect();
    let max_w = child_laid.iter().map(|l| l.rect.w).fold(0.0_f64, f64::max);
    let max_h = child_laid.iter().map(|l| l.rect.h).fold(0.0_f64, f64::max);
    let size = c.clamp(max_w, max_h);
    // Pass 2: re-anchor children inside the resolved frame.
    let (fh, fv) = align.factors();
    for laid in child_laid.iter_mut() {
        let dx = (size.w - laid.rect.w) * fh;
        let dy = (size.h - laid.rect.h) * fv;
        laid.rect.x = origin.0 + dx;
        laid.rect.y = origin.1 + dy;
    }
    LaidOut {
        view: self_view.clone(),
        rect: Rect { x: origin.0, y: origin.1, w: size.w, h: size.h },
        deco: Decoration::default(),
        children: child_laid,
    }
}

// ─── Modified — modifier chain ────────────────────────────────

fn layout_modified(
    child: &View,
    mods: &[Modifier],
    ctx: LayoutCtx,
    origin: (f64, f64),
    c: Constraints,
    self_view: &View,
) -> LaidOut {
    // Apply modifiers outward-to-inward to derive the child's constraints.
    let mut inner_c = c;
    let mut total_pad = EdgesPhys::default();
    let mut frame_spec: Option<FrameSpec> = None;
    let mut bake = Decoration::default();
    let mut offset_phys: (f64, f64) = (0.0, 0.0);

    // Walk mods in order: padding shrinks constraints, frame overrides.
    for m in mods.iter() {
        match m {
            Modifier::Padding(e) => {
                let ep = resolve_edges(e, &ctx, &c);
                total_pad.top    += ep.top;
                total_pad.right  += ep.right;
                total_pad.bottom += ep.bottom;
                total_pad.left   += ep.left;
                inner_c = inner_c.shrink_by_edges(&ep);
            }
            Modifier::Frame(f) => {
                frame_spec = Some(*f);
                // Override constraints if frame specifies width/height.
                if let Some(w) = f.width {
                    let wp = w.resolve_for_axis_with_cell(c.max_w, ctx.scale, ctx.cell_w_phys);
                    inner_c.min_w = wp - total_pad.left - total_pad.right;
                    inner_c.max_w = inner_c.min_w;
                    inner_c.min_w = inner_c.min_w.max(0.0);
                    inner_c.max_w = inner_c.max_w.max(0.0);
                }
                if let Some(h) = f.height {
                    let hp = h.resolve_for_axis_with_cell(c.max_h, ctx.scale, ctx.cell_w_phys);
                    inner_c.min_h = hp - total_pad.top - total_pad.bottom;
                    inner_c.max_h = inner_c.min_h;
                    inner_c.min_h = inner_c.min_h.max(0.0);
                    inner_c.max_h = inner_c.max_h.max(0.0);
                }
                if let Some(min_w) = f.min_w {
                    let v = min_w.resolve_for_axis_with_cell(c.max_w, ctx.scale, ctx.cell_w_phys);
                    inner_c.min_w = inner_c.min_w.max(v);
                }
                if let Some(max_w) = f.max_w {
                    let v = max_w.resolve_for_axis_with_cell(c.max_w, ctx.scale, ctx.cell_w_phys);
                    inner_c.max_w = inner_c.max_w.min(v);
                }
                if let Some(min_h) = f.min_h {
                    let v = min_h.resolve_for_axis_with_cell(c.max_h, ctx.scale, ctx.cell_w_phys);
                    inner_c.min_h = inner_c.min_h.max(v);
                }
                if let Some(max_h) = f.max_h {
                    let v = max_h.resolve_for_axis_with_cell(c.max_h, ctx.scale, ctx.cell_w_phys);
                    inner_c.max_h = inner_c.max_h.min(v);
                }
            }
            Modifier::Offset(x, y) => {
                let xp = x.resolve_for_axis_with_cell(c.max_w, ctx.scale, ctx.cell_w_phys);
                let yp = y.resolve_for_axis_with_cell(c.max_h, ctx.scale, ctx.cell_w_phys);
                offset_phys.0 += xp;
                offset_phys.1 += yp;
            }
            Modifier::Background(c) => bake.bg = Some(*c),
            Modifier::Border(w, c) => {
                let wp = w.resolve_for_axis_with_cell(0.0, ctx.scale, ctx.cell_w_phys);
                bake.border = Some((wp, *c));
            }
            Modifier::CornerRadius(r) => {
                bake.radius = r.resolve_for_axis_with_cell(0.0, ctx.scale, ctx.cell_w_phys);
            }
            Modifier::Shadow(s) => {
                let blur = s.blur.resolve_for_axis_with_cell(0.0, ctx.scale, ctx.cell_w_phys);
                let ox = s.offset.0.resolve_for_axis_with_cell(0.0, ctx.scale, ctx.cell_w_phys);
                let oy = s.offset.1.resolve_for_axis_with_cell(0.0, ctx.scale, ctx.cell_w_phys);
                bake.shadow = Some(DecoShadow { blur, offset: (ox, oy), color: s.color });
            }
            Modifier::Hidden(h) => bake.hidden = *h,
            Modifier::OnClick(id) => bake.on_click = Some(*id),
            Modifier::OnHover(id) => bake.on_hover = Some(*id),
            Modifier::Id(id) => bake.id = Some(*id),
            Modifier::ZIndex(_) => { /* read at paint */ }
        }
    }
    bake.padding = total_pad;

    // Lay out child inside computed inner constraints, at the
    // (origin + padding) corner.
    let child_origin = (
        origin.0 + total_pad.left + offset_phys.0,
        origin.1 + total_pad.top  + offset_phys.1,
    );
    let mut child_laid = layout(child, ctx, child_origin, inner_c);

    // Self size = child size + padding, clamped by outer constraints
    // (frame mins/maxes already baked into inner_c, but we still
    // honour the outer `c`).
    let self_w = child_laid.rect.w + total_pad.left + total_pad.right;
    let self_h = child_laid.rect.h + total_pad.top  + total_pad.bottom;
    let mut size = c.clamp(self_w, self_h);

    // Apply explicit frame size if set (overrides hug-content).
    if let Some(f) = frame_spec {
        if let Some(w) = f.width {
            size.w = w.resolve_for_axis_with_cell(c.max_w, ctx.scale, ctx.cell_w_phys);
        }
        if let Some(h) = f.height {
            size.h = h.resolve_for_axis_with_cell(c.max_h, ctx.scale, ctx.cell_w_phys);
        }
        // Re-position child by frame's align if its size < frame.
        let (fh, fv) = f.align.factors();
        let extra_w = size.w - child_laid.rect.w - total_pad.left - total_pad.right;
        let extra_h = size.h - child_laid.rect.h - total_pad.top  - total_pad.bottom;
        shift_subtree(&mut child_laid, extra_w * fh, extra_h * fv);
    }

    LaidOut {
        view: self_view.clone(),
        rect: Rect { x: origin.0 + offset_phys.0, y: origin.1 + offset_phys.1, w: size.w, h: size.h },
        deco: bake,
        children: vec![child_laid],
    }
}

fn resolve_edges(e: &Edges, ctx: &LayoutCtx, parent_c: &Constraints) -> EdgesPhys {
    EdgesPhys {
        top:    e.top.resolve_for_axis_with_cell(parent_c.max_h, ctx.scale, ctx.cell_w_phys),
        right:  e.right.resolve_for_axis_with_cell(parent_c.max_w, ctx.scale, ctx.cell_w_phys),
        bottom: e.bottom.resolve_for_axis_with_cell(parent_c.max_h, ctx.scale, ctx.cell_w_phys),
        left:   e.left.resolve_for_axis_with_cell(parent_c.max_w, ctx.scale, ctx.cell_w_phys),
    }
}

fn shift_subtree(laid: &mut LaidOut, dx: f64, dy: f64) {
    laid.rect.x += dx;
    laid.rect.y += dy;
    for c in laid.children.iter_mut() {
        shift_subtree(c, dx, dy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::view::{Text, vstack, hstack, spacer};
    use crate::ui::view::types::Distribute;
    use crate::ui::core::Length;

    fn ctx() -> LayoutCtx {
        LayoutCtx { scale: 2.0, cell_w_phys: 16.0, cell_h_phys: 32.0, ascent_phys: 24.0 }
    }

    #[test]
    fn text_size_is_chars_times_cell_w() {
        let v = Text::new("hello").build();  // 5 chars
        let l = layout(&v, ctx(), (0.0, 0.0), Constraints::loose(1000.0, 1000.0));
        assert_eq!(l.rect.w, 5.0 * 16.0);
        assert_eq!(l.rect.h, 32.0); // Body = 1.0 × cell_h
    }

    #[test]
    fn vstack_packs_top_with_gap() {
        let v = vstack(vec![
            Text::new("a").build(),
            Text::new("b").build(),
            Text::new("c").build(),
        ]);
        let mut v = v;
        if let View::VStack { gap, .. } = &mut v {
            *gap = Length::Pt(4.0); // 4pt × scale 2 = 8 phys
        }
        let l = layout(&v, ctx(), (0.0, 0.0), Constraints::loose(1000.0, 1000.0));
        // Three rows of cell_h=32 + 2×gap=16 = 112 phys
        assert_eq!(l.children.len(), 3);
        assert_eq!(l.children[0].rect.y, 0.0);
        assert_eq!(l.children[1].rect.y, 32.0 + 8.0);
        assert_eq!(l.children[2].rect.y, 64.0 + 16.0);
    }

    #[test]
    fn hstack_with_spacer_distributes_leftover() {
        let v = hstack(vec![
            Text::new("a").build(),  // 16 phys wide
            spacer(),                // takes leftover
            Text::new("b").build(),  // 16 phys wide
        ]);
        let l = layout(&v, ctx(), (0.0, 0.0), Constraints::loose(200.0, 100.0));
        // text "a" at x=0; text "b" at x = 200 - 16 = 184
        assert_eq!(l.children[0].rect.x, 0.0);
        assert_eq!(l.children[2].rect.x, 184.0);
    }

    #[test]
    fn padding_shrinks_child_and_grows_self() {
        let inner = Text::new("hi").build(); // 32 phys wide × 32 tall
        let v = inner.padding(Edges::all(Length::Pt(4.0))); // 8 phys / side
        let l = layout(&v, ctx(), (0.0, 0.0), Constraints::loose(1000.0, 1000.0));
        // Self size = child + 2*8 padding
        assert_eq!(l.rect.w, 32.0 + 16.0);
        assert_eq!(l.rect.h, 32.0 + 16.0);
        // Child positioned at (8, 8) from parent's origin
        assert_eq!(l.children[0].rect.x, 8.0);
        assert_eq!(l.children[0].rect.y, 8.0);
    }

    #[test]
    fn frame_overrides_intrinsic_size() {
        let v = Text::new("x").build()
            .frame(FrameSpec { width: Some(Length::Pt(100.0)), ..Default::default() });
        let l = layout(&v, ctx(), (0.0, 0.0), Constraints::loose(1000.0, 1000.0));
        assert_eq!(l.rect.w, 200.0); // 100pt × scale 2
    }

    #[test]
    fn vstack_distribute_center_centres_packed_children() {
        let v = vstack(vec![Text::new("a").build(), Text::new("b").build()]);
        let v = match v {
            View::VStack { children, gap, align, .. } => View::VStack {
                children, gap, align, distribute: Distribute::Center,
            },
            _ => unreachable!(),
        };
        let l = layout(&v, ctx(), (0.0, 0.0), Constraints::tight(50.0, 200.0));
        // Two rows of 32 = 64 total; (200 - 64) / 2 = 68 pad at top
        assert_eq!(l.children[0].rect.y, 68.0);
        assert_eq!(l.children[1].rect.y, 100.0);
    }

    #[test]
    fn zstack_centres_children_when_align_center() {
        use crate::ui::view::zstack;
        use crate::ui::core::Color;
        // 60×40 filled, then 20×10 text on top.
        let big = View::Filled { color: Color::rgba(255,0,0,1.0), radius: Length::Pt(0.0) }
            .frame(FrameSpec { width: Some(Length::Pt(60.0/2.0)), height: Some(Length::Pt(40.0/2.0)), ..Default::default() });
        let small = Text::new("x").build();
        let z = match zstack(vec![big, small]) {
            View::ZStack { children, .. } => View::ZStack { children, align: Anchor::Center },
            _ => unreachable!(),
        };
        let l = layout(&z, ctx(), (0.0, 0.0), Constraints::loose(200.0, 200.0));
        // Big = 60×40, small = 16×32.  Z size = 60×40 (max each axis).
        // Small centred: x = (60-16)/2 = 22, y = (40-32)/2 = 4.
        assert_eq!(l.children[1].rect.x, 22.0);
        assert_eq!(l.children[1].rect.y, 4.0);
    }
}
