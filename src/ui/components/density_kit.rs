//! Drawing one scene at two display scales and holding the second to
//! the first: everything in the frame at scale 2 must be where it is
//! at scale 1, twice as far out and twice as large.
//!
//! The comparison is made on what the renderer hands the encoder — the
//! rectangles, rounded rectangles and glyph quads of a frame — not on
//! pixels, so it is exact and antialiasing does not come into it.
//!
//! ## Tolerances, in pixels of the scale-2 frame
//!
//! - **An edge of a rectangle: 1.**  A position snapped to a whole
//!   pixel at each scale is off by at most half a pixel at either, and
//!   twice the first half is one.
//! - **A rectangle's radius, border and shadow: 0.5.**  They are never
//!   snapped, so they double exactly; the half is for `f32`.
//! - **A side under 3 px at scale 1: 0.5 on its length.**  A line that
//!   stayed as thin as it was would pass the edge rule; this is what
//!   makes it say so.
//! - **An edge of a run of glyphs: 3.**  A glyph's quad is its mask,
//!   the outline's bounds taken out to whole pixels: up to one pixel
//!   at scale 1, which doubles, and up to one more at scale 2.  The
//!   mask's own padding is taken off first.
//!
//! ## What is not doubled
//!
//! **Hairlines.**  A border or a line exactly one pixel thick at both
//! scales is one somebody meant to be the thinnest line the display
//! draws (`ui::theme::spec` lists them).  They are counted, and a
//! scene's count is held to a table, so a new one is a decision and
//! not an accident.
//!
//! **A terminal's grid.**  Its cell is the font's metrics rounded to whole pixels at
//! each scale, so the cell at scale 2 is twice the cell at scale 1 give
//! or take a pixel, and a pane's text is laid out in cells.  What a
//! pane's grid draws is therefore held to its place *in cells*: the
//! same column and row, in the pane's own content area, whose origin
//! does double.

use crate::layout::Layout;
use crate::render_metal::probe::FrameInstances;
use crate::render_metal::MetalRenderer;
use crate::ui::core::Primitive;
use std::cell::{Cell, RefCell};

pub(crate) const RECT_TOL: f64 = 1.0;
pub(crate) const ATTR_TOL: f64 = 0.5;
pub(crate) const GLYPH_TOL: f64 = 3.0;
/// A side shorter than this at scale 1 is a line, and is held to its
/// length exactly.
const THIN: f64 = 3.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Kind {
    Fill,
    Rounded,
    Glyph,
    /// Where a canvas text run starts; it has no extent yet.
    TextOrigin,
}

/// One thing in a frame: its layer, its place in that layer's order,
/// its box as `[left, top, right, bottom]`, and for a rounded
/// rectangle its radius, border width and shadow blur.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Elem {
    pub(crate) layer: &'static str,
    pub(crate) n: usize,
    pub(crate) kind: Kind,
    pub(crate) edges: [f64; 4],
    pub(crate) attrs: [f64; 3],
    /// Its colour, as four bytes in one number: what tells a control
    /// under the pointer from the same control at rest.
    pub(crate) tint: u32,
}

/// A pane's content area: where its grid starts and the cell it is
/// counted in, both as the pane's painter rounds them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PaneArea {
    pub(crate) origin: (f64, f64),
    pub(crate) cell: (f64, f64),
    pub(crate) extent: (f64, f64),
}

/// A frame and what is known about where things are in it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Capture {
    pub(crate) scene: String,
    pub(crate) elems: Vec<Elem>,
    pub(crate) panes: Vec<PaneArea>,
    /// Named places, for saying what an element is: `(name, edges)`.
    pub(crate) regions: Vec<(String, [f64; 4])>,
    /// The window's layout, which is what a click is tested against.
    pub(crate) layout: Option<Layout>,
    /// The terminal's cell, as the font gives it.
    pub(crate) cell: (f64, f64),
    /// The frame as drawn, RGBA, when it was asked for with
    /// [`capture_pixels`]: `(bytes, width, height)`.
    pub(crate) pixels: Option<(Vec<u8>, u32, u32)>,
}

thread_local! {
    static WANT_PIXELS: Cell<bool> = const { Cell::new(false) };
    static CAPTURING: Cell<bool> = const { Cell::new(false) };
    static CAPTURED: RefCell<Option<Capture>> = const { RefCell::new(None) };
}

/// The display scale is one value for the process; tests that move it
/// take turns.
fn scale_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Run `f` with the display scale at `scale`, and put it back.
pub(crate) fn at_scale<T>(scale: f64, f: impl FnOnce() -> T) -> T {
    let _turn = scale_lock();
    let saved = crate::ui::chrome_scale();
    crate::ui::set_chrome_scale(scale);
    let out = f();
    crate::ui::set_chrome_scale(saved);
    out
}

/// Whether the snapshot harness is drawing for a comparison of
/// geometry rather than for a picture.
pub(crate) fn capturing() -> bool {
    CAPTURING.get()
}

/// A baseline's name at the scale in force: `<name>.2x` at scale 2.
pub(crate) fn scaled_name(name: &str) -> String {
    if crate::ui::chrome_scale() == 2.0 { format!("{name}.2x") } else { name.to_string() }
}

/// A window's size in pixels at the scale in force, from its size at
/// scale 1.
pub(crate) fn scaled_size(w: u32, h: u32) -> (u32, u32) {
    let k = crate::ui::chrome_scale();
    ((w as f64 * k).round() as u32, (h as f64 * k).round() as u32)
}

/// Draw `scene` at `scale` and return its frame.  `None` where there
/// is no Metal device to lay a frame out with.
pub(crate) fn capture(scale: f64, scene: impl FnOnce()) -> Option<Capture> {
    at_scale(scale, || {
        CAPTURING.set(true);
        scene();
        CAPTURING.set(false);
        // what a scene put in force for itself is not the next one's
        *crate::frame_build::frame::TEST_OVERLAY.lock().unwrap_or_else(|e| e.into_inner()) = None;
        crate::settings::set_for_test(crate::settings::Settings::default());
        crate::ui::strings::follow_language();
        CAPTURED.take()
    })
}

/// [`capture`], with the frame's pixels as well.
pub(crate) fn capture_pixels(scale: f64, scene: impl FnOnce()) -> Option<Capture> {
    WANT_PIXELS.set(true);
    let frame = capture(scale, scene);
    WANT_PIXELS.set(false);
    frame
}

/// Called by a snapshot harness once its frame is read back.  Keeps
/// the pixels and returns true when the harness is drawing for
/// `capture_pixels`.
pub(crate) fn deliver_pixels(rgba: &[u8], w: u32, h: u32) -> bool {
    if capturing() {
        CAPTURED.with_borrow_mut(|c| {
            if let Some(c) = c {
                c.pixels = Some((rgba.to_vec(), w, h));
            }
        });
    }
    capturing()
}

/// Called by a snapshot harness once its frame is laid out.  Keeps the
/// frame and returns true when the harness is drawing for `capture`
/// and is not wanted for its pixels too.
pub(crate) fn deliver(scene: &str, renderer: &MetalRenderer, layout: &Layout, n_panes: usize) -> bool {
    if !capturing() {
        return false;
    }
    let (cell_w, cell_h) = renderer.cell_dims();
    let (cw, ch) = ((cell_w as f32).round() as f64, (cell_h as f32).round() as f64);
    let panes = layout
        .cells
        .iter()
        .take(n_panes)
        .map(|c| PaneArea {
            origin: (
                ((c.x + layout.padding) as f32).round() as f64,
                ((c.y_top + layout.cell_title_h + layout.padding) as f32).round() as f64,
            ),
            cell: (cw, ch),
            extent: (c.cols as f64 * cw, c.rows as f64 * ch),
        })
        .collect();
    let capture = Capture {
        scene: scene.to_string(),
        elems: elems_of(&renderer.frame_instances()),
        panes,
        regions: regions_of(layout, n_panes),
        layout: Some(layout.clone()),
        cell: (cell_w, cell_h),
        pixels: None,
    };
    CAPTURED.set(Some(capture));
    !WANT_PIXELS.get()
}

fn regions_of(layout: &Layout, n_panes: usize) -> Vec<(String, [f64; 4])> {
    let edges = |x: f64, y: f64, w: f64, h: f64| [x, y, x + w, y + h];
    let of = |r: crate::layout::Rect| edges(r.x, r.y_top, r.w, r.h);
    let mut out = Vec::new();
    const TOOLBAR: [&str; 6] = ["sidebar", "layout", "process monitor", "developer panel", "accounts", "settings"];
    for (name, rect) in TOOLBAR.iter().zip(layout.toolbar_buttons()) {
        if rect.w > 0.0 {
            out.push((format!("the toolbar's {name} button"), of(rect)));
        }
    }
    for (i, rect) in layout.close_session_rects.iter().enumerate() {
        out.push((format!("the close mark of sidebar row {i}"), of(*rect)));
    }
    if layout.add_session_button_rect.w > 0.0 {
        out.push(("the sidebar's add button".to_string(), of(layout.add_session_button_rect)));
    }
    if layout.sidebar_w > 0.0 {
        let row_h = marspot_term::layout::sidebar_row_h();
        let top = layout.top_inset + layout.sidebar_top_pad_phys;
        for i in 0..n_panes {
            out.push((format!("sidebar row {i}"), edges(0.0, top + i as f64 * row_h, layout.sidebar_w, row_h)));
        }
        out.push(("the sidebar".to_string(), edges(0.0, layout.top_inset, layout.sidebar_w, layout.window_h)));
    }
    if layout.top_inset > 0.0 {
        out.push(("the header".to_string(), edges(0.0, 0.0, layout.window_w, layout.top_inset)));
    }
    for (i, c) in layout.cells.iter().enumerate() {
        if layout.cell_title_h > 0.0 {
            out.push((format!("the title strip of pane {i}"), edges(c.x, c.y_top, c.w, layout.cell_title_h)));
        }
        out.push((format!("pane {i}"), edges(c.x, c.y_top, c.w, c.h)));
    }
    out
}

fn elems_of(f: &FrameInstances) -> Vec<Elem> {
    use golia_ui_core::scene::{GlyphInstance, RectInstance, UiRectInstance};
    let edges = |o: [f32; 2], s: [f32; 2]| {
        [o[0] as f64, o[1] as f64, o[0] as f64 + s[0] as f64, o[1] as f64 + s[1] as f64]
    };
    let tint = |c: golia_ui_core::Rgba8| u32::from_le_bytes([c.r, c.g, c.b, c.a]);
    let mut out = Vec::new();
    let mut fills = |layer: &'static str, list: &[RectInstance]| {
        for (n, r) in list.iter().enumerate() {
            out.push(Elem {
                layer,
                n,
                kind: Kind::Fill,
                edges: edges(r.origin, r.size),
                attrs: Default::default(),
                tint: tint(r.color),
            });
        }
    };
    fills("fills", &f.cells);
    fills("overlay fills", &f.overlay_cells);
    let mut rounded = |layer: &'static str, list: &[UiRectInstance]| {
        for (n, r) in list.iter().enumerate() {
            out.push(Elem {
                layer,
                n,
                kind: Kind::Rounded,
                edges: edges(r.origin, r.size),
                attrs: [r.radius as f64, r.border_width as f64, r.shadow_blur as f64],
                tint: tint(r.fill),
            });
        }
    };
    rounded("rounded rects", &f.ui_rects);
    rounded("overlay rounded rects", &f.overlay_ui_rects);
    let mut glyphs = |layer: &'static str, list: &[GlyphInstance]| {
        for (n, g) in list.iter().enumerate() {
            out.push(Elem {
                layer,
                n,
                kind: Kind::Glyph,
                edges: edges(g.origin, g.size),
                attrs: Default::default(),
                tint: tint(g.color),
            });
        }
    };
    glyphs("glyphs", &f.glyphs);
    glyphs("colour glyphs", &f.color_glyphs);
    glyphs("overlay glyphs", &f.overlay_glyphs);
    for (n, prim) in f.menu.iter().enumerate() {
        let layer = "context menu";
        out.push(match prim {
            Primitive::Rect(r) => Elem {
                layer,
                n,
                kind: Kind::Rounded,
                edges: [r.x, r.y, r.x + r.w, r.y + r.h],
                attrs: [r.radius, r.border.map_or(0.0, |b| b.0), r.shadow.map_or(0.0, |s| s.0)],
                tint: tint(crate::frame_build::color::rgba8_of_f32(r.fill.to_rgba_f32())),
            },
            Primitive::Line(l) => Elem {
                layer,
                n,
                kind: Kind::Rounded,
                edges: [l.from.0.min(l.to.0), l.from.1.min(l.to.1), l.from.0.max(l.to.0), l.from.1.max(l.to.1)],
                attrs: [f64::default(), l.width, f64::default()],
                tint: 0,
            },
            Primitive::Text(t) => {
                Elem { layer, n, kind: Kind::TextOrigin, edges: [t.x, t.y, t.x, t.y], attrs: Default::default(), tint: 0 }
            }
        });
    }
    out
}

impl Capture {
    /// What `e` is, as nearly as the frame's named places can say.
    fn describe(&self, e: &Elem) -> String {
        let (cx, cy) = ((e.edges[0] + e.edges[2]) * 0.5, (e.edges[1] + e.edges[3]) * 0.5);
        let place = self
            .regions
            .iter()
            .find(|(_, r)| cx >= r[0] && cx < r[2] && cy >= r[1] && cy < r[3])
            .map_or("the window", |(name, _)| name.as_str());
        format!("{} #{} in {place}", e.layer, e.n)
    }

    /// The pane whose content area holds all of `e`, for the layers a
    /// pane's grid draws into.
    fn pane_holding(&self, e: &Elem) -> Option<usize> {
        if !matches!(e.layer, "fills" | "glyphs" | "colour glyphs") {
            return None;
        }
        // a glyph's mask may hang a pixel or two outside its cell
        let slack = if e.kind == Kind::Glyph { 3.0 } else { 0.5 };
        self.panes.iter().position(|p| {
            e.edges[0] >= p.origin.0 - slack
                && e.edges[1] >= p.origin.1 - slack
                && e.edges[2] <= p.origin.0 + p.extent.0 + slack
                && e.edges[3] <= p.origin.1 + p.extent.1 + slack
        })
    }
}

/// Where `e`, drawn at scale 1 in `one`, belongs in `two`.
fn expected(one: &Capture, two: &Capture, e: &Elem, ratio: f64) -> [f64; 4] {
    match one.pane_holding(e) {
        Some(i) => {
            let (a, b) = (one.panes[i], two.panes[i]);
            let x = |v: f64| b.origin.0 + (v - a.origin.0) * b.cell.0 / a.cell.0;
            let y = |v: f64| b.origin.1 + (v - a.origin.1) * b.cell.1 / a.cell.1;
            [x(e.edges[0]), y(e.edges[1]), x(e.edges[2]), y(e.edges[3])]
        }
        None => e.edges.map(|v| v * ratio),
    }
}

fn fmt(e: [f64; 4]) -> String {
    format!("x {:.1} y {:.1} w {:.1} h {:.1}", e[0], e[1], e[2] - e[0], e[3] - e[1])
}


/// What holding one frame to another found.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Everything that is not where it belongs, in the order the frame
    /// draws it.  Each names its scene and what it is.
    pub(crate) findings: Vec<String>,
    /// Borders and lines one physical pixel thick at both scales.  That
    /// is a decision someone made for each of them, so they are counted
    /// rather than passed: a scene's count is held to a table.
    pub(crate) hairlines: usize,
}

impl Report {
    /// The first finding, as the error of a check.
    pub(crate) fn first(&self) -> Result<(), String> {
        self.findings.first().map_or(Ok(()), |f| Err(f.clone()))
    }
}

/// Hold `two` (drawn at `ratio` times the scale of `one`) to `one`.
pub(crate) fn compare(one: &Capture, two: &Capture, ratio: f64) -> Report {
    let mut report = Report::default();
    let scene = &one.scene;
    for layer in LAYERS {
        let (a, b) = (count(one, layer), count(two, layer));
        if a != b {
            report.findings.push(format!("{scene}: {layer} holds {a} at scale 1 and {b} at scale {ratio}"));
        }
    }
    if !report.findings.is_empty() {
        return report;
    }
    for (e1, e2) in one.elems.iter().zip(&two.elems) {
        if e1.kind == Kind::Glyph {
            continue;
        }
        let want = expected(one, two, e1, ratio);
        let what = one.describe(e1);
        let off = (0..4).map(|i| (e2.edges[i] - want[i]).abs()).fold(0.0, f64::max);
        if off > RECT_TOL + 1e-3 {
            report.findings.push(format!(
                "{scene}: {what} is not scaled: at scale 1 {}, at scale {ratio} {} where {} was expected (off by {off:.2} px)",
                fmt(e1.edges), fmt(e2.edges), fmt(want),
            ));
            continue;
        }
        for (i, name) in ["radius", "border", "shadow"].iter().enumerate() {
            let (a, b) = (e1.attrs[i], e2.attrs[i]);
            if i == 1 && a == HAIRLINE && b == HAIRLINE {
                report.hairlines += 1;
            } else if (b - a * ratio).abs() > ATTR_TOL {
                report.findings.push(format!(
                    "{scene}: the {name} of {what} is not scaled: {a} at scale 1, {b} at scale {ratio} ({})",
                    fmt(e1.edges),
                ));
            }
        }
        for (axis, (lo, hi)) in [("width", (0, 2)), ("height", (1, 3))] {
            let (len1, len2, len_want) =
                (e1.edges[hi] - e1.edges[lo], e2.edges[hi] - e2.edges[lo], want[hi] - want[lo]);
            if len1 == HAIRLINE && len2 == HAIRLINE {
                report.hairlines += 1;
            } else if len1 > 0.0 && len1 < THIN && (len2 - len_want).abs() > ATTR_TOL {
                report.findings.push(format!(
                    "{scene}: the {axis} of {what} is not scaled: {len1} px at scale 1, {len2} px at scale {ratio} ({})",
                    fmt(e1.edges),
                ));
            }
        }
    }
    for layer in LAYERS {
        compare_runs(one, two, layer, ratio, &mut report.findings);
    }
    report
}

/// One physical pixel.
const HAIRLINE: f64 = 1.0;

const LAYERS: [&str; 8] = [
    "fills", "overlay fills", "rounded rects", "overlay rounded rects",
    "glyphs", "colour glyphs", "overlay glyphs", "context menu",
];

fn count(c: &Capture, layer: &str) -> usize {
    c.elems.iter().filter(|e| e.layer == layer).count()
}

/// A glyph's quad without the padding its mask carries: a pixel on
/// each side and a spare column on the right, the same number of
/// pixels at every scale.  A glyph a pane's grid drew is a whole cell
/// and has none.
fn ink(c: &Capture, e: &Elem) -> [f64; 4] {
    if c.pane_holding(e).is_some() {
        return e.edges;
    }
    [e.edges[0] + 1.0, e.edges[1] + 1.0, e.edges[2] - 2.0, e.edges[3] - 1.0]
}

/// Runs of glyphs: consecutive quads on one line, left to right.  The
/// runs are found at scale 1 and the same glyphs are taken as the run
/// at the other scale.
fn compare_runs(one: &Capture, two: &Capture, layer: &str, ratio: f64, out: &mut Vec<String>) {
    let glyphs = |c: &Capture| -> Vec<Elem> {
        c.elems
            .iter()
            .filter(|e| e.layer == layer && e.kind == Kind::Glyph)
            .map(|e| Elem { edges: ink(c, e), ..*e })
            .collect()
    };
    let (a, b) = (glyphs(one), glyphs(two));
    let mut start = 0;
    while start < a.len() {
        let mut end = start + 1;
        while end < a.len() && same_run(&a[end - 1], &a[end]) {
            end += 1;
        }
        let bounds = |run: &[Elem]| {
            run.iter().fold([f64::MAX, f64::MAX, f64::MIN, f64::MIN], |acc, e| {
                [acc[0].min(e.edges[0]), acc[1].min(e.edges[1]), acc[2].max(e.edges[2]), acc[3].max(e.edges[3])]
            })
        };
        let (b1, b2) = (bounds(&a[start..end]), bounds(&b[start..end]));
        let whole = Elem { edges: b1, ..a[start] };
        let want = expected(one, two, &whole, ratio);
        let off = (0..4).map(|i| (b2[i] - want[i]).abs()).fold(0.0, f64::max);
        if off > GLYPH_TOL + 1e-3 {
            out.push(format!(
                "{}: a run of {} glyphs ({}) is not scaled: at scale 1 {}, at scale {ratio} {} where {} was expected (off by {off:.2} px)",
                one.scene, end - start, one.describe(&whole), fmt(b1), fmt(b2), fmt(want),
            ));
        }
        start = end;
    }
}

fn same_run(prev: &Elem, next: &Elem) -> bool {
    let h = (prev.edges[3] - prev.edges[1]).max(next.edges[3] - next.edges[1]).max(1.0);
    let overlap = prev.edges[3].min(next.edges[3]) - prev.edges[1].max(next.edges[1]);
    let gap = next.edges[0] - prev.edges[2];
    overlap > 0.0 && next.edges[0] >= prev.edges[0] && gap < 1.5 * h
}
