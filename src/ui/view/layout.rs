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
#[derive(Clone, Debug)]
pub struct Decoration {
    pub bg: Option<crate::ui::core::Color>,
    pub border: Option<(f64, crate::ui::core::Color)>, // (width_phys, color)
    pub radius: f64, // phys
    pub shadow: Option<DecoShadow>,
    pub padding: EdgesPhys, // phys
    /// Cumulative opacity 0..=1 — multiplies bg/border/text/img
    /// primitive colors emitted by self and descendants.  Default 1.
    pub opacity: f64,
    /// Clip shape applied to descendants.  None = no clip.
    pub clip: Option<super::view::ClipShape>,
    /// `Hidden` — skip paint but keep layout space.  `Collapsed`
    /// = both layout + paint skipped; handled at layout entry by
    /// returning a zero-size rect.
    pub hidden: bool,
    pub on_click: Option<super::types::ActionId>,
    pub on_double_click: Option<super::types::ActionId>,
    pub on_right_click: Option<super::types::ActionId>,
    pub on_hover: Option<super::types::HoverId>,
    pub on_scroll: Option<super::types::ScrollWheelId>,
    pub on_drag_begin: Option<super::types::DragId>,
    pub id: Option<super::types::ViewId>,
    /// Linear gradient background — baked from `Modifier::Background
    /// Gradient`.  Paint emits as a series of solid bands per stop.
    pub bg_gradient: Option<super::view::LinearGradient>,
    /// Accessibility label / role — surfaced via `NSAccessibility`
    /// bridge in v2+;  v1 just preserves the data for inspection /
    /// future routing.
    pub ax_label: Option<String>,
    pub ax_role: Option<super::view::AxRole>,
    /// Keyboard shortcut binding — KeyEquivalent → ActionId.  Host
    /// collects from tree each frame to build a shortcut table.
    pub shortcut: Option<(super::types::KeyEquivalent, super::types::ActionId)>,
    /// Focus ring membership — view participates in Tab navigation.
    pub focus_id: Option<super::types::FocusId>,
    /// Initial-focus marker (at most one per tree).
    pub auto_focus: bool,
    /// Lifecycle hooks — emitted by reconcile() on appear/disappear.
    pub on_appear: Option<super::types::ActionId>,
    pub on_disappear: Option<super::types::ActionId>,
    /// Declarative enter / exit anim spec.
    pub transition: Option<super::types::Transition>,
    /// Transform applied at paint time.  Translate goes through
    /// offset accumulator already;  scale/rotate held for v2+.
    pub transform: Option<super::types::Transform>,
    /// Blend mode held for paint side (v2+ paint applies it).
    pub blend_mode: Option<super::types::BlendMode>,
}

impl Default for Decoration {
    fn default() -> Self {
        Self {
            bg: None,
            border: None,
            radius: 0.0,
            shadow: None,
            padding: EdgesPhys::default(),
            opacity: 1.0,            // 1.0 = fully opaque
            clip: None,
            hidden: false,
            on_click: None,
            on_double_click: None,
            on_right_click: None,
            on_hover: None,
            on_scroll: None,
            on_drag_begin: None,
            id: None,
            bg_gradient: None,
            ax_label: None,
            ax_role: None,
            shortcut: None,
            focus_id: None,
            auto_focus: false,
            on_appear: None,
            on_disappear: None,
            transition: None,
            transform: None,
            blend_mode: None,
        }
    }
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

/// CJK / wide-character aware string-width in cells.  Routes
/// through `marspot_term::grid::char_width` so chrome and the
/// terminal grid agree on what counts as 1 cell vs 2 cells (CJK
/// ideographs, fullwidth forms, emoji, ambiguous-wide when opted
/// in via `MARSPOT_AMBIGUOUS_WIDE=1`).
pub fn text_width_cells(s: &str) -> usize {
    s.chars().map(|c| marspot_term::grid::char_width(c) as usize).sum()
}

/// Run the layout pass.  `origin` is the parent's top-left in phys;
/// `constraints` bounds this node's size.  Returns the laid-out
/// subtree with all rects in phys.
pub fn layout(view: &View, ctx: LayoutCtx, origin: (f64, f64), c: Constraints) -> LaidOut {
    match view {
        View::Text(t) => {
            // Width = display-cells × cell_w (CJK = 2 cells).
            // Height = line_h.  Wrap = single-line for v1.
            let line_h = ctx.line_h_phys(t.size);
            let raw_w = text_width_cells(&t.content) as f64 * ctx.cell_w_phys;
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
        View::ScrollView { child, id } => layout_scroll(child, *id, ctx, origin, c, view),
        View::LazyVStack { items, gap, item_height, id } =>
            layout_lazy_vstack(items, *gap, *item_height, *id, ctx, origin, c, view),
        View::LazyHStack { items, gap, item_width, id } =>
            layout_lazy_hstack(items, *gap, *item_width, *id, ctx, origin, c, view),
        View::Grid { items, cols, col_gap, row_gap, cell_w, cell_h } =>
            layout_grid(items, *cols, *col_gap, *row_gap, *cell_w, *cell_h, ctx, origin, c, view),
        View::VariableGrid { items, tracks_w, tracks_h, gap } =>
            layout_variable_grid(items, tracks_w, tracks_h, *gap, ctx, origin, c, view),
        View::Toggle { id: _ } => {
            // Fixed visual size for the switch: 36 × 18 logical pt.
            let w_pt = 36.0 * ctx.scale;
            let h_pt = 18.0 * ctx.scale;
            let size = c.clamp(w_pt, h_pt);
            LaidOut {
                view: view.clone(),
                rect: Rect { x: origin.0, y: origin.1, w: size.w, h: size.h },
                deco: Decoration::default(),
                children: Vec::new(),
            }
        }
        View::Picker { options, .. } => {
            // Width = sum option widths × cell + padding; height = 24 pt
            let widest = options.iter()
                .map(|s| text_width_cells(s))
                .max().unwrap_or(0) as f64;
            let per_seg_w = (widest + 2.0) * ctx.cell_w_phys + 16.0;
            let total_w = per_seg_w * options.len().max(1) as f64;
            let h_pt = 24.0 * ctx.scale;
            let size = c.clamp(total_w, h_pt);
            LaidOut {
                view: view.clone(),
                rect: Rect { x: origin.0, y: origin.1, w: size.w, h: size.h },
                deco: Decoration::default(),
                children: Vec::new(),
            }
        }
        View::Image(_) => {
            // Hug constraints or full size if unbounded.  Caller is
            // expected to wrap in `.frame(width:, height:)`.
            let w = if c.max_w.is_finite() { c.max_w } else { 64.0 };
            let h = if c.max_h.is_finite() { c.max_h } else { 64.0 };
            let size = c.clamp(w, h);
            LaidOut {
                view: view.clone(),
                rect: Rect { x: origin.0, y: origin.1, w: size.w, h: size.h },
                deco: Decoration::default(),
                children: Vec::new(),
            }
        }
        View::Shape(_) => {
            // Same shape contract as Filled — fills constraints.
            let w = if c.max_w.is_finite() { c.max_w } else { 32.0 };
            let h = if c.max_h.is_finite() { c.max_h } else { 32.0 };
            let size = c.clamp(w, h);
            LaidOut {
                view: view.clone(),
                rect: Rect { x: origin.0, y: origin.1, w: size.w, h: size.h },
                deco: Decoration::default(),
                children: Vec::new(),
            }
        }
        View::Modified { child, mods } => layout_modified(child, mods, ctx, origin, c, view),
    }
}

// ─── LazyVStack ───────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn layout_lazy_vstack(
    items: &[View],
    gap: Length,
    item_height: Length,
    id: super::types::ViewId,
    ctx: LayoutCtx,
    origin: (f64, f64),
    c: Constraints,
    self_view: &View,
) -> LaidOut {
    let gap_phys = gap.resolve_for_axis_with_cell(0.0, ctx.scale, ctx.cell_w_phys);
    let item_h_phys = item_height.resolve_for_axis_with_cell(0.0, ctx.scale, ctx.cell_w_phys);
    let viewport_w = c.max_w;
    let viewport_h = if c.max_h.is_finite() { c.max_h } else { 400.0 };

    // Update / read scroll state (uniform-height list = ScrollView
    // state shape; reuse same key).
    let mut state = super::scroll::scroll_state(id);
    state.content_h = super::lazy::total_height(items.len(), item_h_phys, gap_phys);
    state.viewport_h = viewport_h;
    state.clamp_offset();
    super::scroll::set_scroll_state(id, state);

    let (first, last) = super::lazy::visible_range(
        items.len(), item_h_phys, gap_phys, state.offset_y, viewport_h,
    );

    let mut laid_children = Vec::with_capacity(last.saturating_sub(first));
    for i in first..last {
        let item_y_local = super::lazy::item_y(i, item_h_phys, gap_phys);
        let item_origin = (origin.0, origin.1 + item_y_local - state.offset_y);
        let item_c = Constraints {
            min_w: 0.0, max_w: viewport_w,
            min_h: item_h_phys, max_h: item_h_phys,
        };
        let laid = layout(&items[i], ctx, item_origin, item_c);
        laid_children.push(laid);
    }

    LaidOut {
        view: self_view.clone(),
        rect: Rect { x: origin.0, y: origin.1, w: viewport_w, h: viewport_h },
        deco: Decoration::default(),
        children: laid_children,
    }
}

#[allow(clippy::too_many_arguments)]
fn layout_lazy_hstack(
    items: &[View],
    gap: Length,
    item_width: Length,
    id: super::types::ViewId,
    ctx: LayoutCtx,
    origin: (f64, f64),
    c: Constraints,
    self_view: &View,
) -> LaidOut {
    let gap_phys = gap.resolve_for_axis_with_cell(0.0, ctx.scale, ctx.cell_w_phys);
    let item_w_phys = item_width.resolve_for_axis_with_cell(0.0, ctx.scale, ctx.cell_w_phys);
    let viewport_h = if c.max_h.is_finite() { c.max_h } else { 32.0 };
    let viewport_w = if c.max_w.is_finite() { c.max_w } else { 400.0 };

    let mut state = super::scroll::scroll_state(id);
    state.content_h = super::lazy::total_height(items.len(), item_w_phys, gap_phys);
    state.viewport_h = viewport_w;
    state.clamp_offset();
    super::scroll::set_scroll_state(id, state);

    let (first, last) = super::lazy::visible_range(
        items.len(), item_w_phys, gap_phys, state.offset_y, viewport_w,
    );

    let mut laid_children = Vec::with_capacity(last.saturating_sub(first));
    for i in first..last {
        let item_x_local = super::lazy::item_y(i, item_w_phys, gap_phys);
        let item_origin = (origin.0 + item_x_local - state.offset_y, origin.1);
        let item_c = Constraints {
            min_w: item_w_phys, max_w: item_w_phys,
            min_h: 0.0, max_h: viewport_h,
        };
        let laid = layout(&items[i], ctx, item_origin, item_c);
        laid_children.push(laid);
    }

    LaidOut {
        view: self_view.clone(),
        rect: Rect { x: origin.0, y: origin.1, w: viewport_w, h: viewport_h },
        deco: Decoration::default(),
        children: laid_children,
    }
}

#[allow(clippy::too_many_arguments)]
fn layout_grid(
    items: &[View],
    cols: usize,
    col_gap: Length,
    row_gap: Length,
    cell_w: Length,
    cell_h: Length,
    ctx: LayoutCtx,
    origin: (f64, f64),
    c: Constraints,
    self_view: &View,
) -> LaidOut {
    if cols == 0 {
        return LaidOut {
            view: self_view.clone(),
            rect: Rect { x: origin.0, y: origin.1, w: 0.0, h: 0.0 },
            deco: Decoration::default(),
            children: Vec::new(),
        };
    }
    let col_gap_phys = col_gap.resolve_for_axis_with_cell(0.0, ctx.scale, ctx.cell_w_phys);
    let row_gap_phys = row_gap.resolve_for_axis_with_cell(0.0, ctx.scale, ctx.cell_w_phys);
    let cell_w_phys = cell_w.resolve_for_axis_with_cell(c.max_w, ctx.scale, ctx.cell_w_phys);
    let cell_h_phys = cell_h.resolve_for_axis_with_cell(c.max_h, ctx.scale, ctx.cell_w_phys);

    let mut laid_children = Vec::with_capacity(items.len());
    let mut row = 0usize;
    let mut col = 0usize;
    for item in items.iter() {
        let cx = origin.0 + col as f64 * (cell_w_phys + col_gap_phys);
        let cy = origin.1 + row as f64 * (cell_h_phys + row_gap_phys);
        let item_c = Constraints {
            min_w: cell_w_phys, max_w: cell_w_phys,
            min_h: cell_h_phys, max_h: cell_h_phys,
        };
        let laid = layout(item, ctx, (cx, cy), item_c);
        laid_children.push(laid);
        col += 1;
        if col >= cols {
            col = 0;
            row += 1;
        }
    }
    let rows = if items.is_empty() { 0 } else {
        ((items.len() + cols - 1) / cols).max(1)
    };
    let total_w = cols as f64 * cell_w_phys + cols.saturating_sub(1) as f64 * col_gap_phys;
    let total_h = rows as f64 * cell_h_phys + rows.saturating_sub(1) as f64 * row_gap_phys;

    LaidOut {
        view: self_view.clone(),
        rect: Rect { x: origin.0, y: origin.1, w: total_w, h: total_h },
        deco: Decoration::default(),
        children: laid_children,
    }
}

/// Resolve `GridTrack` list against a total axis length.  Fixed
/// tracks claim their stated size, Flex tracks split leftover by
/// weight, Auto = 32pt × scale fallback for v1.
fn resolve_tracks(tracks: &[super::view::GridTrack], total: f64, gap: f64, ctx: LayoutCtx) -> Vec<f64> {
    let n = tracks.len();
    if n == 0 { return Vec::new(); }
    let gap_total = (n.saturating_sub(1)) as f64 * gap;
    let mut sizes: Vec<f64> = vec![0.0; n];
    let mut flex_weight: u32 = 0;
    let mut fixed_used = 0.0;
    for (i, t) in tracks.iter().enumerate() {
        match t {
            super::view::GridTrack::Fixed(l) => {
                let v = l.resolve_for_axis_with_cell(total, ctx.scale, ctx.cell_w_phys);
                sizes[i] = v;
                fixed_used += v;
            }
            super::view::GridTrack::Auto => {
                // v1: 32pt fallback.
                let v = 32.0 * ctx.scale;
                sizes[i] = v;
                fixed_used += v;
            }
            super::view::GridTrack::Flex(w) => { flex_weight += *w; }
        }
    }
    let leftover = (total - fixed_used - gap_total).max(0.0);
    if flex_weight > 0 {
        for (i, t) in tracks.iter().enumerate() {
            if let super::view::GridTrack::Flex(w) = t {
                sizes[i] = leftover * (*w as f64) / (flex_weight as f64);
            }
        }
    }
    sizes
}

#[allow(clippy::too_many_arguments)]
fn layout_variable_grid(
    items: &[View],
    tracks_w: &[super::view::GridTrack],
    tracks_h: &[super::view::GridTrack],
    gap: (Length, Length),
    ctx: LayoutCtx,
    origin: (f64, f64),
    c: Constraints,
    self_view: &View,
) -> LaidOut {
    let cols = tracks_w.len();
    if cols == 0 {
        return LaidOut {
            view: self_view.clone(),
            rect: Rect { x: origin.0, y: origin.1, w: 0.0, h: 0.0 },
            deco: Decoration::default(),
            children: Vec::new(),
        };
    }
    let col_gap_phys = gap.0.resolve_for_axis_with_cell(0.0, ctx.scale, ctx.cell_w_phys);
    let row_gap_phys = gap.1.resolve_for_axis_with_cell(0.0, ctx.scale, ctx.cell_w_phys);
    let track_widths  = resolve_tracks(tracks_w, c.max_w, col_gap_phys, ctx);
    let needed_rows = (items.len() + cols - 1).max(1) / cols.max(1);

    // tracks_h cycles if underspecified.
    let track_heights: Vec<f64> = (0..needed_rows).map(|r| {
        let t = &tracks_h[r % tracks_h.len().max(1).min(tracks_h.len()).max(1)];
        match t {
            super::view::GridTrack::Fixed(l) =>
                l.resolve_for_axis_with_cell(c.max_h, ctx.scale, ctx.cell_w_phys),
            super::view::GridTrack::Auto    => 32.0 * ctx.scale,
            super::view::GridTrack::Flex(_) => 32.0 * ctx.scale,  // simplified
        }
    }).collect();

    let mut col_offsets = vec![0.0; cols];
    {
        let mut acc = 0.0;
        for i in 0..cols {
            col_offsets[i] = acc;
            acc += track_widths[i] + col_gap_phys;
        }
    }
    let mut row_offsets = vec![0.0; needed_rows];
    {
        let mut acc = 0.0;
        for i in 0..needed_rows {
            row_offsets[i] = acc;
            acc += track_heights[i] + row_gap_phys;
        }
    }

    let mut laid_children = Vec::with_capacity(items.len());
    for (idx, item) in items.iter().enumerate() {
        let r = idx / cols;
        let cidx = idx % cols;
        let cw = track_widths[cidx];
        let ch = track_heights[r];
        let cx = origin.0 + col_offsets[cidx];
        let cy = origin.1 + row_offsets[r];
        let item_c = Constraints {
            min_w: cw, max_w: cw,
            min_h: ch, max_h: ch,
        };
        let laid = layout(item, ctx, (cx, cy), item_c);
        laid_children.push(laid);
    }

    let total_w: f64 = track_widths.iter().sum::<f64>()
        + cols.saturating_sub(1) as f64 * col_gap_phys;
    let total_h: f64 = track_heights.iter().sum::<f64>()
        + needed_rows.saturating_sub(1) as f64 * row_gap_phys;
    LaidOut {
        view: self_view.clone(),
        rect: Rect { x: origin.0, y: origin.1, w: total_w, h: total_h },
        deco: Decoration::default(),
        children: laid_children,
    }
}

// ─── ScrollView ───────────────────────────────────────────────

fn layout_scroll(
    child: &View,
    id: super::types::ViewId,
    ctx: LayoutCtx,
    origin: (f64, f64),
    c: Constraints,
    self_view: &View,
) -> LaidOut {
    // 1. Read current scroll state (offset).  content_h / viewport_h
    //    will be updated after we measure.
    let mut state = super::scroll::scroll_state(id);

    // 2. Lay the child out with bounded width(scroll only vertical)
    //    and unbounded height(content can be arbitrarily tall).
    let inner_c = Constraints {
        min_w: c.min_w,
        max_w: c.max_w,
        min_h: 0.0,
        max_h: f64::INFINITY,
    };
    // Child's origin = our origin in PRE-scroll coords.  We shift
    // its subtree afterwards by -offset_y so painted positions land
    // inside the viewport.
    let mut child_laid = layout(child, ctx, origin, inner_c);

    // 3. Self viewport size = full constraint (fill given space).
    //    Clamp offset to new content extent + viewport height.
    let viewport_w = c.max_w.min(child_laid.rect.w.max(c.min_w));
    let viewport_h = if c.max_h.is_finite() { c.max_h } else { child_laid.rect.h };
    state.content_h = child_laid.rect.h;
    state.viewport_h = viewport_h;
    state.clamp_offset();
    super::scroll::set_scroll_state(id, state);

    // 4. Shift child subtree by -offset_y so painted y = real -
    //    scroll offset.  Self stays at origin.
    if state.offset_y != 0.0 {
        shift_subtree(&mut child_laid, 0.0, -state.offset_y);
    }

    LaidOut {
        view: self_view.clone(),
        rect: Rect { x: origin.0, y: origin.1, w: viewport_w, h: viewport_h },
        deco: Decoration::default(),
        children: vec![child_laid],
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
    // Short-circuit Collapsed — zero-size rect, no child layout.
    if has_collapsed(mods) {
        return LaidOut {
            view: self_view.clone(),
            rect: Rect { x: origin.0, y: origin.1, w: 0.0, h: 0.0 },
            deco: Decoration { hidden: true, ..Decoration::default() },
            children: Vec::new(),
        };
    }
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
                if let Some(r) = f.aspect {
                    apply_aspect(&mut inner_c, r, super::view::AspectMode::Fit);
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
            Modifier::Collapsed(_) => { /* handled at layout entry */ }
            Modifier::Opacity(o) => {
                bake.opacity *= o.clamp(0.0, 1.0);
            }
            Modifier::Clip(shape) => {
                bake.clip = Some(*shape);
            }
            Modifier::AspectRatio(r, mode) => {
                // Re-resolve inner_c to honor ratio.  Done after
                // current inner_c is settled (Padding / Frame
                // already applied); behave like frame.aspect.
                apply_aspect(&mut inner_c, *r, *mode);
            }
            Modifier::OnClick(id) => bake.on_click = Some(*id),
            Modifier::OnDoubleClick(id) => bake.on_double_click = Some(*id),
            Modifier::OnRightClick(id) => bake.on_right_click = Some(*id),
            Modifier::OnScroll(id) => bake.on_scroll = Some(*id),
            Modifier::OnDragBegin(id) => bake.on_drag_begin = Some(*id),
            Modifier::OnHover(id) => bake.on_hover = Some(*id),
            Modifier::Id(id) => bake.id = Some(*id),
            Modifier::ZIndex(_) => { /* read at paint, future */ }
            Modifier::BackgroundGradient(g) => {
                bake.bg_gradient = Some(g.clone());
            }
            Modifier::BackgroundMaterial(_m) => {
                // v1: render as semi-opaque BG_PANEL.  Real macOS
                // vibrancy = v2+ NSVisualEffectView wiring.
                let mut c = crate::ui::theme::color::BG_PANEL;
                c.a = 0.85;
                bake.bg = Some(c);
            }
            Modifier::AccessibilityLabel(s) => {
                bake.ax_label = Some(s.clone());
            }
            Modifier::AccessibilityRole(r) => {
                bake.ax_role = Some(*r);
            }
            Modifier::Shortcut(k, a) => {
                bake.shortcut = Some((*k, *a));
            }
            Modifier::Focusable(id) => {
                bake.focus_id = Some(*id);
            }
            Modifier::AutoFocus => {
                bake.auto_focus = true;
            }
            Modifier::OnAppear(a) => {
                bake.on_appear = Some(*a);
            }
            Modifier::OnDisappear(a) => {
                bake.on_disappear = Some(*a);
            }
            Modifier::Transition(t) => {
                bake.transition = Some(*t);
            }
            Modifier::Transform(t) => {
                // Translate accumulates into offset for v1 paint;
                // scale / rotate are stored for future Metal vertex
                // transform support.
                let tx_phys = t.translate_x * ctx.scale;
                let ty_phys = t.translate_y * ctx.scale;
                offset_phys.0 += tx_phys;
                offset_phys.1 += ty_phys;
                bake.transform = Some(*t);
            }
            Modifier::BlendMode(m) => {
                bake.blend_mode = Some(*m);
            }
            Modifier::Mask(_inner) => {
                // v1: no-op.  Mask shape rendering requires Metal
                // stencil pipeline.  Bake stays empty;  modifier
                // still recorded by hit_test etc. via match.
            }
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

/// Constrain `inner_c` to match aspect ratio `w / h = ratio` per
/// `mode`.  `Fit` shrinks the longer axis so the box fits;  `Fill`
/// grows the shorter axis so the box covers.
fn apply_aspect(inner_c: &mut Constraints, ratio: f64, mode: super::view::AspectMode) {
    if ratio <= 0.0 { return; }
    let w_max = inner_c.max_w;
    let h_max = inner_c.max_h;
    if !w_max.is_finite() || !h_max.is_finite() { return; }
    let current_ratio = w_max / h_max.max(0.001);
    match mode {
        super::view::AspectMode::Fit => {
            if current_ratio > ratio {
                // Too wide — shrink w.
                let new_w = h_max * ratio;
                inner_c.max_w = new_w;
                inner_c.min_w = inner_c.min_w.min(new_w);
            } else {
                // Too tall — shrink h.
                let new_h = w_max / ratio;
                inner_c.max_h = new_h;
                inner_c.min_h = inner_c.min_h.min(new_h);
            }
        }
        super::view::AspectMode::Fill => {
            // Pull the smaller axis up so the ratio is satisfied.
            // Caller should clip if overflow matters.
            if current_ratio > ratio {
                let new_h = w_max / ratio;
                inner_c.min_h = inner_c.min_h.max(new_h);
                inner_c.max_h = inner_c.max_h.max(new_h);
            } else {
                let new_w = h_max * ratio;
                inner_c.min_w = inner_c.min_w.max(new_w);
                inner_c.max_w = inner_c.max_w.max(new_w);
            }
        }
    }
}

/// Check the modifier chain for `Collapsed(true)` — if found, return
/// `true` so the caller short-circuits to a zero-size rect.
fn has_collapsed(mods: &[super::view::Modifier]) -> bool {
    mods.iter().any(|m| matches!(m, super::view::Modifier::Collapsed(true)))
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
