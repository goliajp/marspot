//! Dev panel — the formal workbench for marspot's UI system.
//!
//! Built with the new Canvas API (no `ViewPainter`).  Renders
//! into the dev panel's independent NSWindow's CAMetalLayer via
//! `encode_canvas_into` (see `dev_window::DevWindow::render`).
//!
//! ## Layout
//!
//! ```text
//! ┌──────────────────────────────────────────────────────┐
//! │ [UI] [Tokens] [Components]                           │  tab strip (36 pt)
//! ├──────────┬───────────────────────────────────────────┤
//! │  Colors  │  ━━━ Colors ━━━━━━━━━━━━━━━━━━━           │
//! │  Units   │  [swatch] [swatch] [swatch] ...           │
//! │  Rects   │                                           │
//! │  Lines   │  ━━━ Units ━━━━━━━━━━━━━━━━━━━            │
//! │  Text    │  ▪▪▪ Pt(50)                               │
//! │          │  ▪▪▪▪▪▪ Pt(100)                            │
//! │          │  ...                                       │
//! └──────────┴───────────────────────────────────────────┘
//! ```
//!
//! The left menu is a vertical list of section anchors; for now it
//! renders as static labels (mouse routing into the dev window is
//! still on the follow-up queue).  The right column stacks all
//! section samples — so opening the dev panel shows every UI primitive
//! at once, sized for visual reference.
//!
//! Once mouse routing lands, the left items become click targets +
//! `state.active_section` selects a single section's content for the
//! right column.

use crate::ui::core::{Canvas, Color, Length, ParentRect, Pt};

// ─── Shared layout constants ──────────────────────────────────
// Both `build_dev_panel_canvas` and `hit_test` reference these,
// so click hit-boxes line up with painted rects to the pixel.
pub const TAB_BAR_H_PT: f64 = 36.0;
pub const MENU_W_PT: f64 = 170.0;
pub const TAB_PAD_X_PT: f64 = 16.0;
pub const TAB_TEXT_Y_PT: f64 = 11.0;
pub const MENU_ROW_H_PT: f64 = 28.0;
pub const MENU_TEXT_PAD_X_PT: f64 = 14.0;
pub const MENU_TOP_PAD_PT: f64 = 8.0;
pub const CONTENT_X_PAD_PT: f64 = 20.0;
pub const SECTION_GAP_PT: f64 = 24.0;
/// Sub-item indent (rendered with "  " prefix in label).
pub const MENU_SUB_INDENT_PT: f64 = 10.0;

/// Hit-test result.  Click landed on a tab strip entry or a left-menu
/// row, or nowhere actionable (content area / outside).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevPanelHit {
    Tab(usize),
    Section(usize),
}

/// Compute which tab / section row the click at logical-pt (x_pt, y_pt)
/// hit, or None.  `chrome_cell_w_pt` is the chrome font's cell width
/// in logical pt (same value `build_dev_panel_canvas` uses for tab
/// width math), needed because tab x ranges depend on label widths.
pub fn hit_test(
    state: &DevPanelState,
    chrome_cell_w_pt: f64,
    x_pt: f64,
    y_pt: f64,
) -> Option<DevPanelHit> {
    // Tab strip — full width, 0..TAB_BAR_H_PT vertically.
    if y_pt < TAB_BAR_H_PT && y_pt >= 0.0 {
        let mut tab_x: f64 = 0.0;
        for (label, id) in TAB_LABELS.iter() {
            let text_w = label.chars().count() as f64 * chrome_cell_w_pt;
            let tab_w = text_w + TAB_PAD_X_PT * 2.0;
            if x_pt >= tab_x && x_pt < tab_x + tab_w {
                return Some(DevPanelHit::Tab(*id));
            }
            tab_x += tab_w;
        }
        return None;
    }
    // Left menu — only when we're on the UI tab; other tabs don't
    // render the menu so a click there shouldn't switch sections.
    if state.active_tab != TAB_UI {
        return None;
    }
    if x_pt < MENU_W_PT && y_pt >= TAB_BAR_H_PT {
        let body_y = TAB_BAR_H_PT;
        let local_y = y_pt - body_y - MENU_TOP_PAD_PT;
        if local_y < 0.0 {
            return None;
        }
        let idx = (local_y / MENU_ROW_H_PT).floor() as usize;
        if idx < SECTION_LABELS.len() {
            return Some(DevPanelHit::Section(SECTION_LABELS[idx].1));
        }
    }
    None
}

/// Tab identifiers — used by `state.active_tab` as an integer.
/// Keep this list small; "UI" is the only one actually populated
/// in this commit, the others are placeholders so the tab strip
/// renders a realistic shape.
pub const TAB_UI: usize = 0;
pub const TAB_TOKENS: usize = 1;
pub const TAB_COMPONENTS: usize = 2;
const TAB_LABELS: &[(&str, usize)] = &[
    ("UI", TAB_UI),
    ("Tokens", TAB_TOKENS),
    ("Components", TAB_COMPONENTS),
];

/// Section anchors within the UI tab's left menu.  Same role as
/// `active_tab` but for the menu's vertical list.  "Model" sits at
/// the top because it's the entry-point explanation — read this
/// first, the rest are individual primitive demos.
pub const SECTION_MODEL: usize = 0;
pub const SECTION_L1: usize = 10;
pub const SECTION_L2: usize = 11;
pub const SECTION_L3: usize = 12;
pub const SECTION_L4: usize = 13;
pub const SECTION_L5: usize = 14;
pub const SECTION_L6: usize = 15;
pub const SECTION_COLORS: usize = 1;
pub const SECTION_UNITS: usize = 2;
pub const SECTION_RECTS: usize = 3;
pub const SECTION_LINES: usize = 4;
pub const SECTION_TEXT: usize = 5;
pub const SECTION_FONT_V5: usize = 6;
const SECTION_LABELS: &[(&str, usize)] = &[
    ("Model",              SECTION_MODEL),
    ("  L1 Foundation",    SECTION_L1),
    ("  L2 Box Model",     SECTION_L2),
    ("  L3 Primitives",    SECTION_L3),
    ("  L4 Layout",        SECTION_L4),
    ("  L5 Components",    SECTION_L5),
    ("  L6 Cross-cutting", SECTION_L6),
    ("Colors",             SECTION_COLORS),
    ("Units",              SECTION_UNITS),
    ("Rects",              SECTION_RECTS),
    ("Lines",              SECTION_LINES),
    ("Text",               SECTION_TEXT),
    ("Font v5",            SECTION_FONT_V5),
];

/// Live state for the dev panel.  Owned by L1 (`ShellApp`); the
/// renderer side (dev window) reads it each frame to build a Canvas.
///
/// `scale` is the device pixel ratio at publish time; the host
/// stamps it in just before calling `build_dev_panel_canvas`.
#[derive(Clone, Debug)]
pub struct DevPanelState {
    /// `true` when the panel's NSWindow is shown.
    pub visible: bool,
    /// Position of the panel's top-left in logical points
    /// (`backingScaleFactor`-independent).  Origin is the
    /// containing window's top-left.  Unused now that dev panel is
    /// its own NSWindow (AppKit owns the window frame) — kept for
    /// backward compat with persistence + tests.
    pub origin_pt: (f64, f64),
    /// Size in logical points.  Same caveat as `origin_pt` — AppKit
    /// owns the actual window size now.
    pub size_pt: (f64, f64),
    /// Active tab index (`TAB_UI` / `TAB_TOKENS` / `TAB_COMPONENTS`).
    pub active_tab: usize,
    /// Active section anchor inside the UI tab's left menu.
    /// `SECTION_COLORS` by default.  Used for the menu-row highlight;
    /// the right content column currently stacks ALL sections regardless
    /// (until mouse routing lets the user actually pick).
    pub active_section: usize,
    /// Device pixel ratio.  Defaults to 2.0; the host overwrites it
    /// from the live NSWindow each frame so persisted state across
    /// displays stays correct.
    pub scale: f64,
}

impl Default for DevPanelState {
    fn default() -> Self {
        Self {
            visible: true,
            origin_pt: (60.0, 80.0),
            size_pt: (420.0, 520.0),
            active_tab: TAB_UI,
            // Model first — it's the mental-model overview the rest
            // of the sections individually demo.  Fresh open reads as
            // "what is this thing" rather than "here are some swatches".
            active_section: SECTION_MODEL,
            scale: 2.0,
        }
    }
}

/// Tokens used by the dev panel chrome itself.  These are the colors
/// the panel paints with — separate from the SAMPLES it shows in the
/// Colors section.  Once P3 adds a real theme module these move there.
pub mod tokens {
    use crate::ui::core::Color;

    pub const PANEL_BG: Color = Color::rgba(20, 22, 28, 1.0);
    pub const TAB_BAR_BG: Color = Color::rgba(28, 31, 39, 1.0);
    pub const TAB_ACTIVE_BG: Color = Color::rgba(20, 22, 28, 1.0);
    pub const TAB_ACTIVE_ACCENT: Color = Color::rgba(91, 162, 250, 1.0);
    pub const TAB_INACTIVE_FG: Color = Color::rgba(140, 153, 168, 1.0);
    pub const TAB_ACTIVE_FG: Color = Color::rgba(232, 238, 248, 1.0);
    pub const MENU_BG: Color = Color::rgba(24, 27, 34, 1.0);
    pub const MENU_ROW_ACTIVE_BG: Color = Color::rgba(51, 107, 173, 1.0);
    pub const MENU_ROW_FG: Color = Color::rgba(180, 190, 205, 1.0);
    pub const MENU_ROW_ACTIVE_FG: Color = Color::rgba(245, 248, 252, 1.0);
    pub const DIVIDER: Color = Color::rgba(255, 255, 255, 0.08);
    pub const SECTION_HEADER_FG: Color = Color::rgba(170, 190, 230, 1.0);
    pub const SECTION_BODY_FG: Color = Color::rgba(200, 208, 220, 1.0);
    pub const SAMPLE_HINT_FG: Color = Color::rgba(130, 140, 156, 1.0);
}

/// Build a Canvas of dev-panel primitives for the dev window.  The
/// dev window's NSWindow already gives us title bar, drag, close,
/// shadow, rounded corners — so this canvas paints content only.
///
/// `window_w_phys` / `window_h_phys` = the dev window's content rect
/// in physical pixels.  `chrome_cell_w` / `chrome_cell_h` = the
/// renderer's chrome font cell metrics (used for text width math).
pub fn build_dev_panel_canvas(
    state: &DevPanelState,
    window_w_phys: f64,
    window_h_phys: f64,
    chrome_cell_w: f32,
    chrome_cell_h: f32,
    chrome_ascent: f32,
    fonts: &dyn crate::ui::view::FontMetricsProvider,
) -> Canvas {
    let scale = state.scale;
    let mut canvas = Canvas::new(scale, ParentRect::window(window_w_phys, window_h_phys));

    // Full-window background — Pct(1.0) so it follows resize.
    canvas.rect()
        .at(Length::Pt(0.0), Length::Pt(0.0))
        .size(Length::Pct(1.0), Length::Pct(1.0))
        .fill(tokens::PANEL_BG)
        .draw();

    // Layout constants come from module-level `pub const`s so
    // `hit_test` references the same values.  Re-bound here to short
    // local names so the rest of the function reads cleanly.
    let tab_bar_h = TAB_BAR_H_PT;
    let menu_w = MENU_W_PT;
    let tab_pad_x = TAB_PAD_X_PT;
    let tab_text_y = TAB_TEXT_Y_PT;
    let menu_row_h = MENU_ROW_H_PT;
    let menu_text_pad_x = MENU_TEXT_PAD_X_PT;

    // Cell metrics translated to logical pt — text width math.
    let cell_w_pt = chrome_cell_w as f64 / scale;
    let _cell_h_pt = chrome_cell_h as f64 / scale;

    // ─── Tab strip ─────────────────────────────────────────────
    canvas.rect()
        .at(Length::Pt(0.0), Length::Pt(0.0))
        .size(Length::Pct(1.0), Length::Pt(tab_bar_h))
        .fill(tokens::TAB_BAR_BG)
        .draw();

    let mut tab_x: f64 = 0.0;
    for (label, id) in TAB_LABELS.iter() {
        let text_w = label.chars().count() as f64 * cell_w_pt;
        let tab_w = text_w + tab_pad_x * 2.0;
        let is_active = *id == state.active_tab;
        if is_active {
            // Slight lift via different BG + bottom accent stroke.
            canvas.rect()
                .at(Length::Pt(tab_x), Length::Pt(0.0))
                .size(Length::Pt(tab_w), Length::Pt(tab_bar_h))
                .fill(tokens::TAB_ACTIVE_BG)
                .draw();
            canvas.rect()
                .at(Length::Pt(tab_x), Length::Pt(tab_bar_h - 2.0))
                .size(Length::Pt(tab_w), Length::Pt(2.0))
                .fill(tokens::TAB_ACTIVE_ACCENT)
                .draw();
        }
        canvas.text(
            Length::Pt(tab_x + tab_pad_x),
            Length::Pt(tab_text_y),
            label,
        )
        .color(if is_active { tokens::TAB_ACTIVE_FG } else { tokens::TAB_INACTIVE_FG })
        .draw();
        tab_x += tab_w;
    }
    // Hairline below the tab strip.
    canvas.line(
        (Length::Pt(0.0), Length::Pt(tab_bar_h)),
        (Length::Pct(1.0), Length::Pt(tab_bar_h)),
    )
    .stroke(Pt(1.0), tokens::DIVIDER)
    .draw();

    // Only the UI tab is populated for now — Tokens / Components are
    // placeholder labels showing the strip's shape.
    if state.active_tab != TAB_UI {
        canvas.text(
            Length::Pt(20.0),
            Length::Pt(tab_bar_h + 24.0),
            "(placeholder — coming next)",
        )
        .color(tokens::SAMPLE_HINT_FG)
        .draw();
        return canvas;
    }

    // ─── UI tab: left menu + right content ─────────────────────
    let body_y = tab_bar_h;

    // Left menu BG.
    canvas.rect()
        .at(Length::Pt(0.0), Length::Pt(body_y))
        .size(Length::Pt(menu_w), Length::Pct(1.0))
        .fill(tokens::MENU_BG)
        .draw();
    // Menu / content divider.
    canvas.line(
        (Length::Pt(menu_w), Length::Pt(body_y)),
        (Length::Pt(menu_w), Length::Pct(1.0)),
    )
    .stroke(Pt(1.0), tokens::DIVIDER)
    .draw();

    // Menu rows.
    for (i, (label, id)) in SECTION_LABELS.iter().enumerate() {
        let row_y = body_y + MENU_TOP_PAD_PT + (i as f64) * menu_row_h;
        let is_active = *id == state.active_section;
        if is_active {
            canvas.rect()
                .at(Length::Pt(6.0), Length::Pt(row_y))
                .size(Length::Pt(menu_w - 12.0), Length::Pt(menu_row_h - 4.0))
                .fill(tokens::MENU_ROW_ACTIVE_BG)
                .radius(Pt(4.0))
                .draw();
        }
        canvas.text(
            Length::Pt(menu_text_pad_x),
            Length::Pt(row_y + 6.0),
            label,
        )
        .color(if is_active { tokens::MENU_ROW_ACTIVE_FG } else { tokens::MENU_ROW_FG })
        .draw();
    }

    // ─── Right content: only the active section ─────────────────
    // Mouse routing into the dev window lets the menu pick a single
    // section — show just that one in the content column so the
    // canvas isn't a 5-section scroll the user can't navigate.
    let content_x = menu_w + CONTENT_X_PAD_PT;
    let y = body_y + 16.0;

    // Render any of the L1..L6 sections OR the overview Model section
    // through the new View tree pipeline.  Old Colors/Units/Rects/
    // Lines/Text sections still use legacy canvas builders.
    let render_view_section = |canvas: &mut Canvas, title: &str, view: crate::ui::view::View| {
        let y = draw_section_header(canvas, content_x, y, title);
        let ctx = crate::ui::view::LayoutCtx {
            scale,
            cell_w_phys: chrome_cell_w as f64,
            cell_h_phys: chrome_cell_h as f64,
            ascent_phys: chrome_ascent as f64,
            fonts,
        };
        let avail_w_pt = (window_w_phys / scale) - content_x - 16.0;
        let avail_h_pt = (window_h_phys / scale) - y;
        let laid = crate::ui::view::layout_view(
            &view, ctx,
            (content_x * scale, y * scale),
            crate::ui::view::Constraints::loose(
                avail_w_pt * scale,
                avail_h_pt * scale,
            ),
        );
        crate::ui::view::paint_into(canvas, &laid, ctx);
    };

    match state.active_section {
        SECTION_MODEL => render_view_section(&mut canvas, "v3 Model — overview", build_model_view()),
        SECTION_L1    => render_view_section(&mut canvas, "L1 — Foundation",     build_l1_view()),
        SECTION_L2    => render_view_section(&mut canvas, "L2 — Box Model",      build_l2_view()),
        SECTION_L3    => render_view_section(&mut canvas, "L3 — Primitives",     build_l3_view()),
        SECTION_L4    => render_view_section(&mut canvas, "L4 — Layout",         build_l4_view()),
        SECTION_L5    => render_view_section(&mut canvas, "L5 — Components",     build_l5_view()),
        SECTION_L6    => render_view_section(&mut canvas, "L6 — Cross-cutting",  build_l6_view()),
        SECTION_COLORS => {
            let y = draw_section_header(&mut canvas, content_x, y, "Colors");
            let _ = draw_colors_sample(&mut canvas, content_x, y, cell_w_pt);
        }
        SECTION_UNITS => {
            let y = draw_section_header(&mut canvas, content_x, y, "Units");
            let _ = draw_units_sample(&mut canvas, content_x, y);
        }
        SECTION_RECTS => {
            let y = draw_section_header(&mut canvas, content_x, y, "Rects");
            let _ = draw_rects_sample(&mut canvas, content_x, y);
        }
        SECTION_LINES => {
            let y = draw_section_header(&mut canvas, content_x, y, "Lines");
            let _ = draw_lines_sample(&mut canvas, content_x, y);
        }
        SECTION_TEXT => {
            let y = draw_section_header(&mut canvas, content_x, y, "Text");
            let _ = draw_text_sample(&mut canvas, content_x, y);
        }
        SECTION_FONT_V5 => {
            render_view_section(&mut canvas, "Font v5 showcase", build_font_v5_view());
        }
        _ => {
            canvas.text(Length::Pt(content_x), Length::Pt(y),
                "(unknown section — internal active_section out of range)")
                .color(tokens::SAMPLE_HINT_FG)
                .draw();
        }
    }

    canvas
}

/// Section header: bold-ish label + hairline underline.  Returns the
/// new y cursor (header_y + header_h).  Header height ~22 pt.
fn draw_section_header(canvas: &mut Canvas, x: f64, y: f64, label: &str) -> f64 {
    canvas.text(Length::Pt(x), Length::Pt(y), label)
        .color(tokens::SECTION_HEADER_FG)
        .draw();
    canvas.line(
        (Length::Pt(x), Length::Pt(y + 18.0)),
        (Length::Pct(1.0), Length::Pt(y + 18.0)),
    )
    .stroke(Pt(1.0), tokens::DIVIDER)
    .draw();
    y + 26.0
}

/// v2 Model section — built with the **new View tree + Modifier
/// chain + Constraints layout + paint pipeline** end-to-end.  This
/// is the self-referential demo: the surface that *describes* the
/// model is *built with* the model.
///
/// Layout shape:
///
/// ```text
/// L1 Foundation
///   text + small Length/Color/Token examples
/// L2 Box Model
///   demo box (bg + border + radius + shadow) labelled
/// L3 Primitives (Canvas)
///   text only — Canvas is described, not shown (the whole panel
///   IS Canvas)
/// L4 View Tree (new)
///   atoms / containers / modifiers as text + tiny VStack/HStack/
///   ZStack demos side-by-side
/// L5 Components
///   text list
/// ```
fn build_model_view() -> crate::ui::view::View {
    use crate::ui::view::{
        Text, Edges, FrameSpec, AspectMode,
        LinearGradient, GradientDir, MaterialStyle,
        vstack, hstack, zstack, filled, hairline_horiz, spacer,
        toggle, picker, grid,
        shape_circle, shape_capsule, shape_rounded_rect,
        scroll_view, ViewId,
    };
    use crate::ui::theme::{color, space, radius, text, elev};
    use crate::ui::core::{Length, Color};

    // Helpers using TextStyle tokens (P3l) ----------------------
    let h1   = |s: &str| Text::new(s).style(text::HEADER).build();
    let body = |s: &str| Text::new(s).style(text::BODY).build();
    let mono = |s: &str| Text::new(s).style(text::CODE).build();
    let hint = |s: &str| Text::new(s).style(text::HINT).build();

    let mk_swatch = |c| {
        filled(c).corner_radius(radius::SM).frame(FrameSpec {
            width:  Some(Length::Pt(14.0)),
            height: Some(Length::Pt(14.0)),
            ..Default::default()
        })
    };

    // Status hints — colored tags so reader sees what's live vs
    // planned at a glance.  Mark per line with [✓] / [v1 待补] / [v2+].
    let ok      = |s: &str| Text::new(s).color(color::SUCCESS).build();
    let todo_v1 = |s: &str| Text::new(s).color(color::WARN).build();
    let todo_v2 = |s: &str| Text::new(s).style(text::HINT).build();

    // ── Visual demo helpers — used multiple times below ──────
    // Length demo bar of given width pt + label.
    let len_bar = |w_pt: f64, c: Color, label: &'static str| -> crate::ui::view::View {
        hstack(vec![
            filled(c).corner_radius(radius::SM).frame(FrameSpec {
                width: Some(Length::Pt(w_pt)),
                height: Some(Length::Pt(10.0)),
                ..Default::default()
            }),
            hint(label),
        ]).hstack_gap(Length::Pt(8.0)).align_cross_center()
    };
    // Space ruler at the given pt width.
    let space_ruler = |w: Length, label: &'static str| {
        hstack(vec![
            filled(color::ACCENT_DIM).frame(FrameSpec {
                width: Some(w),
                height: Some(Length::Pt(6.0)),
                ..Default::default()
            }),
            hint(label),
        ]).hstack_gap(Length::Pt(8.0)).align_cross_center()
    };
    // Radius demo dot.
    let radius_chip = |r: Length, label: &'static str| {
        hstack(vec![
            filled(color::ACCENT).corner_radius(r).frame(FrameSpec {
                width: Some(Length::Pt(24.0)),
                height: Some(Length::Pt(24.0)),
                ..Default::default()
            }),
            hint(label),
        ]).hstack_gap(Length::Pt(6.0)).align_cross_center()
    };
    // Opacity demo column.
    let opacity_chip = |o: f64, label: &'static str| {
        vstack(vec![
            filled(color::ACCENT).corner_radius(radius::SM).frame(FrameSpec {
                width: Some(Length::Pt(28.0)),
                height: Some(Length::Pt(28.0)),
                ..Default::default()
            }).opacity(o),
            hint(label),
        ]).vstack_gap(Length::Pt(4.0)).align_cross_center()
    };
    // Elevation card.
    let elev_card = |s, label: &'static str| {
        vstack(vec![
            Text::new("E").style(text::BODY).build()
                .padding(Edges::all(Length::Pt(8.0)))
                .background(color::BG_RAISED)
                .corner_radius(radius::MD)
                .shadow(s)
                .frame(FrameSpec {
                    width: Some(Length::Pt(36.0)),
                    height: Some(Length::Pt(36.0)),
                    ..Default::default()
                }),
            hint(label),
        ]).vstack_gap(Length::Pt(4.0)).align_cross_center()
    };
    // Distribute demo.
    let dist_demo = |d, label: &'static str| {
        use crate::ui::view::Distribute;
        let _ = d;
        let _ = Distribute::Start;
        let small = || filled(color::ACCENT).corner_radius(radius::SM).frame(FrameSpec {
            width: Some(Length::Pt(12.0)),
            height: Some(Length::Pt(12.0)),
            ..Default::default()
        });
        let row = hstack(vec![small(), small(), small()])
            .distribute(d)
            .frame(FrameSpec {
                width: Some(Length::Pt(96.0)),
                height: Some(Length::Pt(14.0)),
                ..Default::default()
            })
            .background(color::BG_PANEL)
            .corner_radius(radius::SM);
        vstack(vec![row, hint(label)]).vstack_gap(Length::Pt(4.0)).align_cross_center()
    };

    // ── L1 Foundation ────────────────────────────────────────
    let l1 = vstack(vec![
        h1("L1 — Foundation"),
        hstack(vec![
            ok("[✓]"),
            body("Length:  Pt(N) | Pct(F) | Ch(N)"),
        ]).hstack_gap(Length::Pt(6.0)),
        hint("    Pt(1) ≡ CSS 1px   scale-independent"),
        hint("    Pct(0.5) ≡ 50%    of parent axis"),
        hint("    Ch(3) = 3 chrome cells   terminal-domain"),
        hstack(vec![
            ok("[✓]"),
            body("Color:   Color::rgba(r, g, b, a)   ≡ CSS rgba()"),
        ]).hstack_gap(Length::Pt(6.0)),
        hstack(vec![
            ok("[✓]"),
            mono("Tokens:"),
            mk_swatch(color::FG),
            mk_swatch(color::ACCENT),
            mk_swatch(color::SUCCESS),
            mk_swatch(color::WARN),
            mk_swatch(color::DANGER),
            hint("FG/ACCENT/SUCCESS/WARN/DANGER"),
        ]).hstack_gap(Length::Pt(6.0)).align_cross_center(),
        hint("    space:: XS(4) SM(8) MD(12) LG(16) XL(24)"),
        hint("    radius:: SM(3) MD(6) LG(10) PILL(9999)"),
        // Length 视觉 demo —— 三种单位实际宽度对比.
        hint("    实际 Length 渲染对比 ↓"),
        len_bar(40.0, color::ACCENT, "Pt(40)"),
        len_bar(80.0, color::SUCCESS, "Pt(80)"),
        len_bar(120.0, color::WARN,   "Pt(120)"),
        hint("    Pct(F) 跟容器宽自动算,Ch(N) = N × cell_w"),
        // space 标尺
        hint("    space 标尺(横条宽 = 标记尺度):"),
        hstack(vec![
            space_ruler(space::XS,  "XS(4)"),
            space_ruler(space::SM,  "SM(8)"),
            space_ruler(space::MD,  "MD(12)"),
            space_ruler(space::LG,  "LG(16)"),
            space_ruler(space::XL,  "XL(24)"),
            space_ruler(space::XXL, "XXL(32)"),
        ]).hstack_gap(Length::Pt(8.0)).align_cross_center(),
        // radius chip
        hint("    radius chip(同尺寸方块不同 corner_radius):"),
        hstack(vec![
            radius_chip(radius::NONE, "NONE"),
            radius_chip(radius::SM,   "SM(3)"),
            radius_chip(radius::MD,   "MD(6)"),
            radius_chip(radius::LG,   "LG(10)"),
            radius_chip(radius::PILL, "PILL"),
        ]).hstack_gap(Length::Pt(10.0)).align_cross_center(),
        hstack(vec![
            ok("[✓]"),
            body("Identity / HostState map + Lifecycle reconcile"),
        ]).hstack_gap(Length::Pt(6.0)),
        hint("    HashMap<(ViewId, TypeId), Box<dyn Any>>;同 id 可挂多种状态"),
        hint("    reconcile() 收集树里所有 id,drop 失踪的(on_disappear)"),
    ]).vstack_gap(Length::Pt(2.0));

    // ── L2 Box Model — actual box demo ───────────────────────
    use crate::ui::view::Shadow;
    let box_demo = Text::new("content").color(color::FG).build()
        .padding(Edges::all(space::MD))
        .background(color::BG_PANEL)
        .border(Length::Pt(1.0), color::BORDER)
        .corner_radius(radius::MD)
        .shadow(Shadow {
            blur: Length::Pt(6.0),
            offset: (Length::Pt(0.0), Length::Pt(2.0)),
            color: color::SHADOW,
        });
    let l2 = vstack(vec![
        h1("L2 — Box Model"),
        hstack(vec![
            box_demo,
            spacer(),
            vstack(vec![
                hstack(vec![
                    ok("[✓]"),
                    hint("padding / border / radius / shadow"),
                ]).hstack_gap(Length::Pt(6.0)),
                hint("    box-sizing: border-box   inside-stroke   NO margin"),
                hstack(vec![
                    ok("[✓]"),
                    hint("opacity / clip / aspect_ratio"),
                ]).hstack_gap(Length::Pt(6.0)),
                hstack(vec![
                    ok("[✓]"),
                    hint("Material backdrop API (v1 = 半透明 BG;真 NSVisualEffectView 留 v2+)"),
                ]).hstack_gap(Length::Pt(6.0)),
                hstack(vec![
                    ok("[✓]"),
                    hint("LinearGradient (16 bands;真 gradient primitive 留)"),
                ]).hstack_gap(Length::Pt(6.0)),
                hstack(vec![
                    todo_v2("[v2+]"),
                    hint("mask / transform / blend mode / 真 Metal scissor clip"),
                ]).hstack_gap(Length::Pt(6.0)),
            ]).vstack_gap(Length::Pt(2.0)),
        ]).hstack_gap(Length::Pt(16.0)).align_cross_center(),
        // Opacity 视觉 demo
        hint("    .opacity(x) — alpha 累乘下:"),
        hstack(vec![
            opacity_chip(1.0,  "1.0"),
            opacity_chip(0.75, "0.75"),
            opacity_chip(0.5,  "0.5"),
            opacity_chip(0.25, "0.25"),
            opacity_chip(0.1,  "0.1"),
        ]).hstack_gap(Length::Pt(12.0)).align_cross_center(),
        // Gradient demo
        hint("    LinearGradient(TopToBottom + LeftToRight, 16 bands):"),
        hstack(vec![
            filled(color::BG_PANEL).corner_radius(radius::MD)
                .background_gradient(LinearGradient {
                    stops: vec![(0.0, color::ACCENT), (1.0, color::SUCCESS)],
                    direction: GradientDir::TopToBottom,
                })
                .frame(FrameSpec {
                    width: Some(Length::Pt(60.0)),
                    height: Some(Length::Pt(40.0)),
                    ..Default::default()
                }),
            filled(color::BG_PANEL).corner_radius(radius::MD)
                .background_gradient(LinearGradient {
                    stops: vec![(0.0, color::DANGER), (0.5, color::WARN), (1.0, color::SUCCESS)],
                    direction: GradientDir::LeftToRight,
                })
                .frame(FrameSpec {
                    width: Some(Length::Pt(120.0)),
                    height: Some(Length::Pt(40.0)),
                    ..Default::default()
                }),
        ]).hstack_gap(Length::Pt(12.0)).align_cross_center(),
        // Material demo (placeholder)
        hint("    Material backdrop(v1 半透明 BG;真 vibrancy = v2+):"),
        hstack(vec![
            Text::new("Regular").style(text::CAPTION).build()
                .padding(Edges::all(space::SM))
                .background_material(MaterialStyle::Regular)
                .corner_radius(radius::MD),
            Text::new("Thick").style(text::CAPTION).build()
                .padding(Edges::all(space::SM))
                .background_material(MaterialStyle::Thick)
                .corner_radius(radius::MD),
            Text::new("Thin").style(text::CAPTION).build()
                .padding(Edges::all(space::SM))
                .background_material(MaterialStyle::Thin)
                .corner_radius(radius::MD),
        ]).hstack_gap(Length::Pt(12.0)).align_cross_center(),
        // Elevation ladder
        hint("    Elevation(elev::E0..E3 token shadow ladder):"),
        hstack(vec![
            elev_card(elev::E0, "E0"),
            elev_card(elev::E1, "E1"),
            elev_card(elev::E2, "E2"),
            elev_card(elev::E3, "E3"),
        ]).hstack_gap(Length::Pt(20.0)).align_cross_center(),
        // AspectRatio demo
        hint("    .aspect_ratio(2.0, Fit) — 2:1 比:"),
        hstack(vec![
            filled(color::ACCENT_DIM).corner_radius(radius::SM)
                .aspect_ratio(2.0, AspectMode::Fit)
                .frame(FrameSpec {
                    width: Some(Length::Pt(80.0)),
                    height: Some(Length::Pt(60.0)),
                    ..Default::default()
                }),
            hint("80×60 frame + aspect 2:1 = 80×40 实际"),
        ]).hstack_gap(Length::Pt(12.0)).align_cross_center(),
    ]).vstack_gap(Length::Pt(4.0));

    // ── L3 Primitives ────────────────────────────────────────
    let l3 = vstack(vec![
        h1("L3 — Primitives (Canvas)"),
        hstack(vec![
            ok("[✓]"),
            body("rect / line / text"),
        ]).hstack_gap(Length::Pt(6.0)),
        hint("    .fill/.border/.radius/.shadow chain;submission order = z order"),
        hstack(vec![
            ok("[✓]"),
            body("Image / Gradient(Linear)/ Shape(Circle/Capsule/RoundedRect)"),
        ]).hstack_gap(Length::Pt(6.0)),
        hint("    type 定义全 land,Shape paint 通过 rounded-rect 近似"),
        hint("    real Image primitive + Path = v2+ Metal pipeline 工作"),
        hint("    this entire panel IS Canvas — what you see, you can build"),
        // Shape 视觉 demo
        hint("    Shape views(Circle / Capsule / RoundedRect):"),
        hstack(vec![
            shape_circle(color::DANGER).frame(FrameSpec {
                width: Some(Length::Pt(28.0)),
                height: Some(Length::Pt(28.0)),
                ..Default::default()
            }),
            shape_capsule(color::SUCCESS).frame(FrameSpec {
                width: Some(Length::Pt(56.0)),
                height: Some(Length::Pt(20.0)),
                ..Default::default()
            }),
            shape_rounded_rect(Length::Pt(8.0), color::ACCENT).frame(FrameSpec {
                width: Some(Length::Pt(40.0)),
                height: Some(Length::Pt(28.0)),
                ..Default::default()
            }),
        ]).hstack_gap(Length::Pt(12.0)).align_cross_center(),
    ]).vstack_gap(Length::Pt(2.0));

    // ── L4 View Tree — text + 3 mini stack demos side-by-side ─
    let mini = |c| filled(c).corner_radius(radius::SM).frame(FrameSpec {
        width:  Some(Length::Pt(16.0)),
        height: Some(Length::Pt(16.0)),
        ..Default::default()
    });
    let vstack_demo = vstack(vec![
        mini(color::DANGER),
        mini(color::SUCCESS),
        mini(color::ACCENT),
    ]).vstack_gap(Length::Pt(4.0));
    let hstack_demo = hstack(vec![
        mini(color::DANGER),
        mini(color::SUCCESS),
        mini(color::ACCENT),
    ]).hstack_gap(Length::Pt(4.0));
    // ZStack: 3 overlapping squares with offset.
    let zstack_demo = zstack(vec![
        mini(color::DANGER).frame(FrameSpec {
            width: Some(Length::Pt(28.0)),
            height: Some(Length::Pt(28.0)),
            ..Default::default()
        }),
        mini(color::SUCCESS).offset(Length::Pt(8.0), Length::Pt(8.0)),
        mini(color::ACCENT).offset(Length::Pt(16.0), Length::Pt(16.0)),
    ]);

    let l4 = vstack(vec![
        h1("L4 — View Tree + Layout"),
        hstack(vec![
            ok("[✓]"),
            body("Atoms: Text / Spacer / Filled / Hairline"),
        ]).hstack_gap(Length::Pt(6.0)),
        hstack(vec![
            ok("[✓]"),
            body("Containers: VStack / HStack / ZStack"),
        ]).hstack_gap(Length::Pt(6.0)),
        hstack(vec![
            ok("[✓]"),
            body("Modifiers: .padding/.background/.border/.corner_radius"),
        ]).hstack_gap(Length::Pt(6.0)),
        hint("              .shadow/.frame/.offset/.on_click/.id"),
        // Three mini demos in a row.
        hstack(vec![
            vstack(vec![hint("VStack"), vstack_demo]).vstack_gap(Length::Pt(4.0)).align_cross_center(),
            spacer(),
            vstack(vec![hint("HStack"), hstack_demo]).vstack_gap(Length::Pt(4.0)).align_cross_center(),
            spacer(),
            vstack(vec![hint("ZStack"), zstack_demo]).vstack_gap(Length::Pt(4.0)).align_cross_center(),
            spacer(),
        ]).align_cross_start(),
        hint("    Constraints two-pass / AlignCross / Distribute / Anchor (9)"),
        // Distribute 5 mode demo
        hint("    Distribute(主轴分布 5 mode):"),
        hstack(vec![
            dist_demo(crate::ui::view::Distribute::Start,   "Start"),
            dist_demo(crate::ui::view::Distribute::Center,  "Center"),
            dist_demo(crate::ui::view::Distribute::End,     "End"),
            dist_demo(crate::ui::view::Distribute::Spaced,  "Spaced"),
            dist_demo(crate::ui::view::Distribute::Between, "Between"),
        ]).hstack_gap(Length::Pt(10.0)).align_cross_start(),
        hstack(vec![
            ok("[✓]"),
            body("Containers: ScrollView / LazyVStack / LazyHStack / Grid(均匀)"),
        ]).hstack_gap(Length::Pt(6.0)),
        hint("    sidebar / process panel / search overlay 长列表都可虚拟化"),
        // Grid demo - 4x2 colored squares
        hint("    Grid 4 cols × 8 cells uniform demo:"),
        grid(
            (0..8).map(|i| {
                let c = match i % 4 {
                    0 => color::ACCENT,
                    1 => color::SUCCESS,
                    2 => color::WARN,
                    _ => color::DANGER,
                };
                filled(c).corner_radius(radius::SM)
            }).collect(),
            4,
            Length::Pt(20.0),
            Length::Pt(20.0),
            Length::Pt(4.0),
        ),
        hstack(vec![
            todo_v2("[v2+]"),
            hint("    Grid 真完整 spec(variable tracks / span / template-areas)"),
        ]).hstack_gap(Length::Pt(6.0)),
        hstack(vec![
            ok("[✓]"),
            body("Gesture: hit_test_* + InputEvent + DragInProgress 状态机"),
        ]).hstack_gap(Length::Pt(6.0)),
        hint("    InputEvent enum(Click/DoubleClick/RightClick/DragBegin/Move/End/Hover/Scroll)"),
        hint("    DragInProgress { drag_id, started_at, current, modifiers, delta() }"),
        hint("    ActionId / ScrollWheelId / DragId 跟 host reducer 派发(elm-y)"),
        hstack(vec![
            ok("[✓]"),
            body("Stateful views: Toggle / Picker(纯渲染)"),
        ]).hstack_gap(Length::Pt(6.0)),
        hint("    state 存 HostState[ViewId]::ToggleState / PickerState"),
        // Visual Toggle / Picker — 注意是真渲染,如要切要先有 click 派发
        hint("    Toggle 渲染演示(默认 off / on 看不同视觉);Picker 选 1):"),
        hstack(vec![
            toggle(ViewId(0xDE7_1001)),  // default off
            toggle(ViewId(0xDE7_1002)),
            picker(ViewId(0xDE7_1003), vec!["Dark", "Light", "Auto"]),
        ]).hstack_gap(Length::Pt(16.0)).align_cross_center(),
        hint("    Anim<T> + Lerp trait + AnimCurve(Linear/EaseIn/Out/InOut)"),
        hstack(vec![
            todo_v2("[v2+]"),
            body("TextField (NSTextInputClient + IME) / 真 frame schedule"),
        ]).hstack_gap(Length::Pt(6.0)),
        hstack(vec![
            todo_v2("[v2+]"),
            body("Keyboard shortcut + Focus chain"),
        ]).hstack_gap(Length::Pt(6.0)),
    ]).vstack_gap(Length::Pt(2.0));

    // ── L5 Components ────────────────────────────────────────
    let l5 = vstack(vec![
        h1("L5 — Components"),
        hstack(vec![
            ok("[✓]"),
            body("DevPanel.Model section (this panel)"),
        ]).hstack_gap(Length::Pt(6.0)),
        hstack(vec![
            todo_v1("[v1 待补]"),
            body("TabStrip / ContextMenu / Tooltip / Card / Panel"),
        ]).hstack_gap(Length::Pt(6.0)),
        hstack(vec![
            todo_v1("[v1 待补]"),
            body("Sidebar / Table / LayoutModal / SearchOverlay / ProcessMonitor"),
        ]).hstack_gap(Length::Pt(6.0)),
        hstack(vec![
            todo_v1("[v1 待补]"),
            body("DevPanel 主框架(tab + menu + content area)"),
        ]).hstack_gap(Length::Pt(6.0)),
        hint("    完后 P3j ViewPainter 退役 — 净 -400 LOC"),
    ]).vstack_gap(Length::Pt(2.0));

    // ── L6 Cross-cutting ─────────────────────────────────────
    let l6 = vstack(vec![
        h1("L6 — Cross-cutting"),
        hstack(vec![
            ok("[✓]"),
            body("Lifecycle: reconcile(LaidOut) — on_disappear 实施"),
        ]).hstack_gap(Length::Pt(6.0)),
        hint("    每帧 build 完调 reconcile;drop 失踪 id 的 HostState slot"),
        hstack(vec![
            ok("[✓]"),
            body("Accessibility: .accessibility_label / role(modifier API)"),
        ]).hstack_gap(Length::Pt(6.0)),
        hint("    bake 进 LaidOut.deco;真接 NSAccessibility 留 v2+"),
        hstack(vec![
            ok("[✓]"),
            body("Theme: ThemeId + theme::current() + Light token 数据"),
        ]).hstack_gap(Length::Pt(6.0)),
        hint("    AtomicU8 全局;Dark + Light 调色板;themed::color::* 闭包查"),
        hstack(vec![
            todo_v2("[v2+]"),
            hint("    HighContrast + 真 theme swap redraw 钩子"),
        ]).hstack_gap(Length::Pt(6.0)),
        hstack(vec![
            ok("[✓]"),
            body("Animation data: Anim<T> + Lerp trait + AnimCurve"),
        ]).hstack_gap(Length::Pt(6.0)),
        hint("    Linear / EaseIn / EaseOut / EaseInOut 4 curves;Color/f64 已实 Lerp"),
        hstack(vec![
            todo_v2("[v2+]"),
            body("    真 frame 调度 + .transition() + 不破坏 idle CPU=0"),
        ]).hstack_gap(Length::Pt(6.0)),
        hstack(vec![
            todo_v2("[v2+]"),
            body("i18n / RTL  (Leading/Trailing 命名已留 RTL 接口)"),
        ]).hstack_gap(Length::Pt(6.0)),
    ]).vstack_gap(Length::Pt(2.0));

    // ── Footer — link to doc ─────────────────────────────────
    let footer = vstack(vec![
        hint("docs/ui-system-model.md — 完整设计 v3"),
        hint("18 章 + 完整 LOC roadmap + SOTA self-assessment"),
    ]).vstack_gap(Length::Pt(2.0));

    // ── Hairline separators between layers ───────────────────
    let sep = || hairline_horiz(color::DIVIDER)
        .frame(FrameSpec {
            height: Some(Length::Pt(1.0)),
            width: Some(Length::Pct(1.0)),
            ..Default::default()
        });

    // ViewId 给 dev panel Model section 的 ScrollView.  arbitrary
    // u32,只要全局唯一即可 — 这里用 magic number 标 dev-panel/
    // model 路径(便于 grep).
    let model_scroll_id = ViewId(0xDE7_0001);

    // Seed Toggle / Picker demo state so the visual differs from
    // default-off / index-0.  Inserted once into HostState;  no
    // visual logic depends on them being "live" — just illustrative.
    use crate::ui::view::{ToggleState, PickerState, with_host_state_mut};
    with_host_state_mut(|s| {
        // The second Toggle in the demo row appears "on".
        if s.get::<ToggleState>(ViewId(0xDE7_1002)).is_none() {
            s.insert(ViewId(0xDE7_1002), ToggleState { on: true });
        }
        // Picker selects "Light"(index 1)by default to be visible.
        if s.get::<PickerState>(ViewId(0xDE7_1003)).is_none() {
            s.insert(ViewId(0xDE7_1003), PickerState { selected: 1 });
        }
    });

    let content = vstack(vec![
        l1,
        sep(),
        l2,
        sep(),
        l3,
        sep(),
        l4,
        sep(),
        l5,
        sep(),
        l6,
        sep(),
        footer,
    ]).vstack_gap(Length::Pt(8.0))
    .frame(FrameSpec {
        width: Some(Length::Pct(1.0)),
        ..Default::default()
    })
    .padding(Edges::only(
        Length::Pt(0.0),
        Length::Pt(16.0),
        Length::Pt(16.0),
        Length::Pt(0.0),
    ));

    scroll_view(model_scroll_id, content)
}

/// Public — the dev_window's wheel handler needs to know which
/// `ViewId` to apply scroll deltas to.  Kept in sync with the id
/// `build_model_view` embeds.
pub const DEV_PANEL_MODEL_SCROLL_ID: crate::ui::view::ViewId =
    crate::ui::view::ViewId(0xDE7_0001);

/// Map an `active_section` constant to the `ViewId` of its
/// ScrollView so the wheel handler can deliver to the right page.
pub fn scroll_id_for_section(active: usize) -> crate::ui::view::ViewId {
    use crate::ui::view::ViewId;
    match active {
        SECTION_L1 => ViewId(0xDE7_0010),
        SECTION_L2 => ViewId(0xDE7_0020),
        SECTION_L3 => ViewId(0xDE7_0030),
        SECTION_L4 => ViewId(0xDE7_0040),
        SECTION_L5 => ViewId(0xDE7_0050),
        SECTION_L6 => ViewId(0xDE7_0060),
        _          => DEV_PANEL_MODEL_SCROLL_ID,
    }
}

// ─── Module-level helpers for L1..L6 detailed pages ───────────
//
// Each `build_l#_view()` builds its layer's content using these
// helpers + the public view module surface.  Pages are scrollable.

mod h {
    use crate::ui::view::{
        Text, View, Edges, FrameSpec, ToggleState, PickerState,
        toggle, picker, filled, hstack, vstack, with_host_state_mut,
        Modifier, ViewId,
    };
    use crate::ui::theme::{color, space, radius, text};
    use crate::ui::core::{Length, Color};

    pub fn h2(s: &str) -> View {
        Text::new(s).style(text::LARGE_HEADER).build()
    }
    pub fn h3(s: &str) -> View {
        Text::new(s).style(text::HEADER).build()
    }
    pub fn body(s: &str) -> View {
        Text::new(s).style(text::BODY).build()
    }
    pub fn mono(s: &str) -> View {
        Text::new(s).style(text::CODE).build()
    }
    pub fn hint(s: &str) -> View {
        Text::new(s).style(text::HINT).build()
    }
    pub fn ok(s: &str) -> View {
        Text::new(s).color(color::SUCCESS).build()
    }
    pub fn todo_v1(s: &str) -> View {
        Text::new(s).color(color::WARN).build()
    }
    pub fn todo_v2(s: &str) -> View {
        Text::new(s).style(text::HINT).build()
    }

    /// `[✓] label` row.
    pub fn done_row(label: &str) -> View {
        hstack(vec![ok("[✓]"), body(label)]).hstack_gap(Length::Pt(6.0))
    }
    /// `[v1 待补] label` row.
    pub fn v1_row(label: &str) -> View {
        hstack(vec![todo_v1("[v1 待补]"), body(label)]).hstack_gap(Length::Pt(6.0))
    }
    /// `[v2+] label` row.
    pub fn v2_row(label: &str) -> View {
        hstack(vec![todo_v2("[v2+]"), body(label)]).hstack_gap(Length::Pt(6.0))
    }
    /// Sub-explanation under a status row.
    pub fn sub(s: &str) -> View {
        hint(&format!("    {s}"))
    }

    pub fn swatch(c: Color) -> View {
        filled(c).corner_radius(radius::SM).frame(FrameSpec {
            width: Some(Length::Pt(14.0)),
            height: Some(Length::Pt(14.0)),
            ..Default::default()
        })
    }

    pub fn seed_toggle(id: u32, on: bool) {
        with_host_state_mut(|s| {
            if s.get::<ToggleState>(ViewId(id)).is_none() {
                s.insert(ViewId(id), ToggleState { on });
            }
        });
    }
    pub fn seed_picker(id: u32, sel: usize) {
        with_host_state_mut(|s| {
            if s.get::<PickerState>(ViewId(id)).is_none() {
                s.insert(ViewId(id), PickerState { selected: sel });
            }
        });
    }

    pub fn scrollable_page(scroll_id: u32, body: Vec<View>) -> View {
        use crate::ui::view::scroll_view;
        // 12pt vstack gap = roomy spacing per user request
        // ("items 垂直间距稍微加大").
        let content = vstack(body)
            .vstack_gap(Length::Pt(12.0))
            .frame(FrameSpec { width: Some(Length::Pct(1.0)), ..Default::default() })
            .padding(Edges::only(
                Length::Pt(0.0),
                Length::Pt(16.0),
                Length::Pt(20.0),
                Length::Pt(0.0),
            ));
        scroll_view(ViewId(scroll_id), content)
    }
}

// ─── L1 Foundation ────────────────────────────────────────────

fn build_l1_view() -> crate::ui::view::View {
    use crate::ui::view::{Text, FrameSpec, vstack, hstack, filled};
    use crate::ui::theme::{color, space, radius, text};
    use crate::ui::core::Length;
    let len_bar = |w: f64, c, lab: &'static str| {
        hstack(vec![
            filled(c).corner_radius(radius::SM).frame(FrameSpec {
                width: Some(Length::Pt(w)), height: Some(Length::Pt(10.0)),
                ..Default::default()
            }),
            h::hint(lab),
        ]).hstack_gap(Length::Pt(8.0)).align_cross_center()
    };
    let space_ruler = |w, lab: &'static str| {
        hstack(vec![
            filled(color::ACCENT_DIM).frame(FrameSpec {
                width: Some(w), height: Some(Length::Pt(6.0)),
                ..Default::default()
            }),
            h::hint(lab),
        ]).hstack_gap(Length::Pt(8.0)).align_cross_center()
    };
    let radius_chip = |r, lab: &'static str| {
        vstack(vec![
            filled(color::ACCENT).corner_radius(r).frame(FrameSpec {
                width: Some(Length::Pt(28.0)), height: Some(Length::Pt(28.0)),
                ..Default::default()
            }),
            h::hint(lab),
        ]).vstack_gap(Length::Pt(4.0)).align_cross_center()
    };

    h::scrollable_page(0xDE7_0010, vec![
        h::h2("Length"),
        h::done_row("Pt(N) | Pct(F) | Ch(N)"),
        h::sub("Pt(1) ≡ CSS 1px (scale-independent;乘 scale 得 phys)"),
        h::sub("Pct(F) = F × 父轴.scale-independent."),
        h::sub("Ch(N) = N × chrome cell_w(terminal 域专用)"),
        h::hint("    实际渲染对比 ↓"),
        len_bar(40.0,  color::ACCENT,  "Pt(40)"),
        len_bar(80.0,  color::SUCCESS, "Pt(80)"),
        len_bar(120.0, color::WARN,    "Pt(120)"),

        h::h2("Color"),
        h::done_row("Color::rgba(r, g, b, a) ≡ CSS rgba()"),
        h::sub("u8 通道 + f64 alpha;Lerp impl 已 land 用于 Anim<Color>"),

        h::h2("Tokens v4"),
        h::done_row("Semantic palette + space + radius + elev + text style + motion + border + layer"),
        h::done_row("UI color 完整:FG{,_MUTED,_DISABLED,_INVERSE,_LINK} + SURFACE_0..4 + OVERLAY"),
        h::done_row("Tab/Sidebar/Status/Diff 命名空间(TAB_ACTIVE_BG / SIDEBAR_BG / DIFF_ADD_BG / ...)"),
        h::done_row("4-level severity(INFO/SUCCESS/WARN/DANGER/CRITICAL)+ FOCUS_RING + DISABLED_*"),
        h::done_row("Terminal palette terminal::{BG, FG, CURSOR_BG/FG, SELECTION_BG/FG, LINK, BOLD_FG}"),
        h::done_row("Terminal ansi 16(black/red/green/yellow/blue/magenta/cyan/white × normal+bright)"),
        h::done_row("Terminal search::{MATCH, MATCH_CURRENT}"),
        h::done_row("motion::{INSTANT, FAST, NORMAL, SLOW, VERY_SLOW} + motion::curve::*"),
        h::done_row("border::{NONE, HAIRLINE, THIN, MEDIUM, THICK}"),
        h::done_row("layer::{CONTENT, STATUS, STICKY, TOOLBAR, POPOVER, MODAL, TOAST, TOOLTIP, SYSTEM}"),
        h::done_row("themed::{color::*, terminal::{ansi, search}::*} 全闭包覆盖 Dark/Light/HC"),
        hstack(vec![
            h::mono("color::"),
            h::swatch(color::FG),
            h::swatch(color::FG_MUTED),
            h::swatch(color::BG),
            h::swatch(color::BG_RAISED),
            h::swatch(color::BORDER),
            h::swatch(color::ACCENT),
            h::swatch(color::ACCENT_DIM),
            h::swatch(color::SUCCESS),
            h::swatch(color::WARN),
            h::swatch(color::DANGER),
        ]).hstack_gap(Length::Pt(6.0)).align_cross_center(),
        h::hint("    FG/FG_MUTED/BG/BG_RAISED/BORDER/ACCENT/ACCENT_DIM/SUCCESS/WARN/DANGER"),
        hstack(vec![
            h::mono("space::"),
            space_ruler(space::XS,  "XS(4)"),
            space_ruler(space::SM,  "SM(8)"),
            space_ruler(space::MD,  "MD(12)"),
            space_ruler(space::LG,  "LG(16)"),
            space_ruler(space::XL,  "XL(24)"),
            space_ruler(space::XXL, "XXL(32)"),
        ]).hstack_gap(Length::Pt(10.0)).align_cross_center(),
        hstack(vec![
            h::mono("radius::"),
            radius_chip(radius::NONE, "NONE"),
            radius_chip(radius::SM,   "SM(3)"),
            radius_chip(radius::MD,   "MD(6)"),
            radius_chip(radius::LG,   "LG(10)"),
            radius_chip(radius::PILL, "PILL"),
        ]).hstack_gap(Length::Pt(12.0)).align_cross_center(),

        h::h2("Font 分离(PTY vs UI)"),
        h::done_row("MetalRenderer::{chrome_font_metrics, terminal_font_metrics, ui_font_metrics}"),
        h::sub("UI 走 ui_font_metrics;Terminal grid 走 terminal_font_metrics — 当前两者 = chrome"),
        h::done_row("MARSPOT_UI_FONT_SCALE env var(0.0..4.0,默认 1.0)"),
        h::sub("layout 端尺寸适配已 land;真 glyph 在 UI size rasterize = B0.7(FontCache 扩展)"),
        h::v2_row("不同 font 真切换(MARSPOT_UI_FONT_NAME — 加载独立 CTFont)"),

        h::h2("Identity / HostState / Lifecycle"),
        h::done_row("HashMap<(ViewId, TypeId), Box<dyn Any>>"),
        h::sub("一个 view id 可挂多种状态类型(Scroll/Toggle/Picker/TextField/...)"),
        h::sub("HOST_STATE thread_local;with_host_state(_mut) 闭包入口"),
        h::done_row("reconcile(LaidOut) — on_disappear 实施"),
        h::sub("遍历 tree 收集 id;retain HOST_STATE 中存在的;drop 失踪"),
        h::v2_row("on_appear hook"),
        h::sub("当 stateful view 类型加 init hook 时绑(eg TextField 加载光标位)"),
    ])
}

// ─── L2 Box Model ─────────────────────────────────────────────

fn build_l2_view() -> crate::ui::view::View {
    use crate::ui::view::{
        Text, Edges, FrameSpec, AspectMode,
        LinearGradient, GradientDir, MaterialStyle,
        vstack, hstack, filled,
    };
    use crate::ui::theme::{color, space, radius, text, elev};
    use crate::ui::core::Length;
    let box_demo = Text::new("content").color(color::FG).build()
        .padding(Edges::all(space::MD))
        .background(color::BG_PANEL)
        .border(Length::Pt(1.0), color::BORDER)
        .corner_radius(radius::MD)
        .shadow(elev::E2);
    let opacity_chip = |o: f64, lab: &'static str| {
        vstack(vec![
            filled(color::ACCENT).corner_radius(radius::SM).frame(FrameSpec {
                width: Some(Length::Pt(32.0)), height: Some(Length::Pt(32.0)),
                ..Default::default()
            }).opacity(o),
            h::hint(lab),
        ]).vstack_gap(Length::Pt(4.0)).align_cross_center()
    };
    let elev_card = |s, lab: &'static str| {
        vstack(vec![
            Text::new("E").style(text::BODY).build()
                .padding(Edges::all(Length::Pt(8.0)))
                .background(color::BG_RAISED)
                .corner_radius(radius::MD)
                .shadow(s)
                .frame(FrameSpec {
                    width: Some(Length::Pt(40.0)),
                    height: Some(Length::Pt(40.0)),
                    ..Default::default()
                }),
            h::hint(lab),
        ]).vstack_gap(Length::Pt(4.0)).align_cross_center()
    };

    h::scrollable_page(0xDE7_0020, vec![
        h::h2("Box decoration"),
        h::done_row("padding / border / corner_radius / shadow"),
        h::sub("border-box 永远;inside-stroke border;NO margin(父级 padding/gap)"),
        box_demo,
        h::hint("    ↑ padding MD + border 1pt + radius MD + shadow E2"),

        h::h2("Opacity"),
        h::done_row(".opacity(0.0..=1.0) — alpha 累乘下子树"),
        hstack(vec![
            opacity_chip(1.0,  "1.0"),
            opacity_chip(0.75, "0.75"),
            opacity_chip(0.5,  "0.5"),
            opacity_chip(0.25, "0.25"),
            opacity_chip(0.1,  "0.1"),
        ]).hstack_gap(Length::Pt(14.0)).align_cross_center(),

        h::h2("Clip"),
        h::done_row(".clip(ClipShape::Rect | RoundedRect(r))"),
        h::sub("v1 = viewport culling(rect 完全外 skip);真 Metal scissor = v2+"),
        h::v2_row("真 pixel-clip(round rect mask / 任意 path)"),

        h::h2("Aspect ratio"),
        h::done_row(".aspect_ratio(w/h, Fit | Fill)"),
        hstack(vec![
            filled(color::ACCENT_DIM).corner_radius(radius::SM)
                .aspect_ratio(2.0, AspectMode::Fit)
                .frame(FrameSpec {
                    width: Some(Length::Pt(80.0)),
                    height: Some(Length::Pt(60.0)),
                    ..Default::default()
                }),
            h::hint("80×60 + aspect 2:1 Fit → 80×40 实际"),
        ]).hstack_gap(Length::Pt(12.0)).align_cross_center(),
        hstack(vec![
            filled(color::SUCCESS).corner_radius(radius::SM)
                .aspect_ratio(0.5, AspectMode::Fit)
                .frame(FrameSpec {
                    width: Some(Length::Pt(80.0)),
                    height: Some(Length::Pt(60.0)),
                    ..Default::default()
                }),
            h::hint("80×60 + aspect 1:2 Fit → 30×60 实际"),
        ]).hstack_gap(Length::Pt(12.0)).align_cross_center(),

        h::h2("Gradient background"),
        h::done_row(".background_gradient(LinearGradient { stops, direction })"),
        h::sub("v1 = 16-band approx;真 Gradient primitive = v2+ Metal pipeline"),
        hstack(vec![
            filled(color::BG_PANEL).corner_radius(radius::MD)
                .background_gradient(LinearGradient {
                    stops: vec![(0.0, color::ACCENT), (1.0, color::SUCCESS)],
                    direction: GradientDir::TopToBottom,
                })
                .frame(FrameSpec {
                    width: Some(Length::Pt(70.0)),
                    height: Some(Length::Pt(48.0)),
                    ..Default::default()
                }),
            filled(color::BG_PANEL).corner_radius(radius::MD)
                .background_gradient(LinearGradient {
                    stops: vec![
                        (0.0, color::DANGER),
                        (0.5, color::WARN),
                        (1.0, color::SUCCESS),
                    ],
                    direction: GradientDir::LeftToRight,
                })
                .frame(FrameSpec {
                    width: Some(Length::Pt(140.0)),
                    height: Some(Length::Pt(48.0)),
                    ..Default::default()
                }),
        ]).hstack_gap(Length::Pt(12.0)).align_cross_center(),

        h::h2("Material backdrop"),
        h::done_row(".background_material(Regular | Thick | Thin)"),
        h::sub("v1 = 半透明 BG_PANEL fallback;真 NSVisualEffectView vibrancy = v2+"),
        hstack(vec![
            Text::new("Regular").style(text::CAPTION).build()
                .padding(Edges::all(space::SM))
                .background_material(MaterialStyle::Regular)
                .corner_radius(radius::MD),
            Text::new("Thick").style(text::CAPTION).build()
                .padding(Edges::all(space::SM))
                .background_material(MaterialStyle::Thick)
                .corner_radius(radius::MD),
            Text::new("Thin").style(text::CAPTION).build()
                .padding(Edges::all(space::SM))
                .background_material(MaterialStyle::Thin)
                .corner_radius(radius::MD),
        ]).hstack_gap(Length::Pt(12.0)).align_cross_center(),

        h::h2("Elevation ladder"),
        h::done_row("token::elev::{E0, E1, E2, E3} — Material-style shadow"),
        hstack(vec![
            elev_card(elev::E0, "E0"),
            elev_card(elev::E1, "E1"),
            elev_card(elev::E2, "E2"),
            elev_card(elev::E3, "E3"),
        ]).hstack_gap(Length::Pt(20.0)).align_cross_center(),

        h::h2("Hidden / Collapsed"),
        h::done_row(".hidden(true) — 不画但占空间;.collapsed(true) — zero-size + 不画"),
        h::sub("CSS 类比:visibility: hidden;vs display: none"),

        h::done_row(".transform(Transform) — translate v1 已 paint,scale/rotate 数据保留"),
        h::done_row(".mask(View) / .blend_mode(BlendMode) — modifier API 已落"),
        h::sub("paint 端:mask 需 Metal stencil;scale/rotate 需 vertex transform — v2+"),
        h::v2_row("真 Metal scissor pixel-clip(替换 culling)+ mask/transform paint"),
    ])
}

// ─── L3 Primitives ────────────────────────────────────────────

fn build_l3_view() -> crate::ui::view::View {
    use crate::ui::view::{
        FrameSpec,
        vstack, hstack,
        shape_circle, shape_capsule, shape_rounded_rect,
    };
    use crate::ui::theme::{color, radius};
    use crate::ui::core::Length;

    h::scrollable_page(0xDE7_0030, vec![
        h::h2("Canvas atoms"),
        h::done_row("rect / line / text"),
        h::sub("submission order = z order;builder chain .fill/.border/.radius/.shadow"),
        h::sub("整个 dev panel 都是 Canvas primitives — 你看到的就是 builder 链出来的"),

        h::h2("Shape views"),
        h::done_row("Circle / Capsule / RoundedRect"),
        h::sub("v1 走 rounded-rect 近似;真 SDF Path = v2+ Metal pipeline"),
        hstack(vec![
            vstack(vec![
                shape_circle(color::DANGER).frame(FrameSpec {
                    width: Some(Length::Pt(36.0)), height: Some(Length::Pt(36.0)),
                    ..Default::default()
                }),
                h::hint("Circle"),
            ]).vstack_gap(Length::Pt(4.0)).align_cross_center(),
            vstack(vec![
                shape_capsule(color::SUCCESS).frame(FrameSpec {
                    width: Some(Length::Pt(72.0)), height: Some(Length::Pt(24.0)),
                    ..Default::default()
                }),
                h::hint("Capsule"),
            ]).vstack_gap(Length::Pt(4.0)).align_cross_center(),
            vstack(vec![
                shape_rounded_rect(Length::Pt(8.0), color::ACCENT).frame(FrameSpec {
                    width: Some(Length::Pt(56.0)), height: Some(Length::Pt(36.0)),
                    ..Default::default()
                }),
                h::hint("RoundedRect"),
            ]).vstack_gap(Length::Pt(4.0)).align_cross_center(),
        ]).hstack_gap(Length::Pt(16.0)).align_cross_center(),

        h::h2("Image view"),
        h::done_row("Image { source, mode, tint } API"),
        h::sub("4 sources: Glyph / Raw(Arc<Vec<u8>>) / IOSurface / Named"),
        h::sub("v1 paint = tinted rect placeholder;真 Image primitive = v2+"),

        h::v2_row("Path SDF / 任意 vector shape / freeform stroke"),
        h::sub("需要 Metal pipeline 加 path mesh 或 SDF rendering"),
    ])
}

// ─── L4 Layout ────────────────────────────────────────────────

fn build_l4_view() -> crate::ui::view::View {
    use crate::ui::view::{
        FrameSpec, Distribute,
        vstack, hstack, zstack, filled, spacer, grid, toggle, picker,
        ViewId,
    };
    use crate::ui::theme::{color, radius};
    use crate::ui::core::Length;

    let mini = |c| filled(c).corner_radius(radius::SM).frame(FrameSpec {
        width:  Some(Length::Pt(16.0)),
        height: Some(Length::Pt(16.0)),
        ..Default::default()
    });
    let vs_demo = vstack(vec![mini(color::DANGER), mini(color::SUCCESS), mini(color::ACCENT)])
        .vstack_gap(Length::Pt(4.0));
    let hs_demo = hstack(vec![mini(color::DANGER), mini(color::SUCCESS), mini(color::ACCENT)])
        .hstack_gap(Length::Pt(4.0));
    let zs_demo = zstack(vec![
        mini(color::DANGER).frame(FrameSpec {
            width: Some(Length::Pt(28.0)), height: Some(Length::Pt(28.0)),
            ..Default::default()
        }),
        mini(color::SUCCESS).offset(Length::Pt(8.0), Length::Pt(8.0)),
        mini(color::ACCENT).offset(Length::Pt(16.0), Length::Pt(16.0)),
    ]);
    let dist_demo = |d, lab: &'static str| {
        let small = || filled(color::ACCENT).corner_radius(radius::SM).frame(FrameSpec {
            width: Some(Length::Pt(12.0)), height: Some(Length::Pt(12.0)),
            ..Default::default()
        });
        vstack(vec![
            hstack(vec![small(), small(), small()])
                .distribute(d)
                .frame(FrameSpec {
                    width: Some(Length::Pt(110.0)), height: Some(Length::Pt(16.0)),
                    ..Default::default()
                })
                .background(color::BG_PANEL)
                .corner_radius(radius::SM),
            h::hint(lab),
        ]).vstack_gap(Length::Pt(4.0)).align_cross_center()
    };

    h::seed_toggle(0xDE7_1002, true);
    h::seed_picker(0xDE7_1003, 1);

    h::scrollable_page(0xDE7_0040, vec![
        h::h2("View enum + Modifier chain"),
        h::done_row("Atoms: Text / Spacer / Filled / Hairline / Image / Shape"),
        h::done_row("Containers: VStack / HStack / ZStack"),
        hstack(vec![
            vstack(vec![h::hint("VStack"), vs_demo]).vstack_gap(Length::Pt(6.0)).align_cross_center(),
            spacer(),
            vstack(vec![h::hint("HStack"), hs_demo]).vstack_gap(Length::Pt(6.0)).align_cross_center(),
            spacer(),
            vstack(vec![h::hint("ZStack"), zs_demo]).vstack_gap(Length::Pt(6.0)).align_cross_center(),
            spacer(),
        ]).align_cross_start(),

        h::h2("Constraints two-pass"),
        h::done_row("parent → constraints → child returns size(Flutter 同形)"),
        h::sub("Pass A 量 intrinsic;Pass B 算 spacer flex;Pass C 摆位 + tight constraints"),

        h::h2("AlignCross / Distribute / Anchor"),
        h::done_row("AlignCross: Start / Center / End / Stretch"),
        h::done_row("Distribute: Start / Center / End / Spaced / Between"),
        hstack(vec![
            dist_demo(Distribute::Start,   "Start"),
            dist_demo(Distribute::Center,  "Center"),
            dist_demo(Distribute::End,     "End"),
            dist_demo(Distribute::Spaced,  "Spaced"),
            dist_demo(Distribute::Between, "Between"),
        ]).hstack_gap(Length::Pt(10.0)).align_cross_start(),
        h::done_row("Anchor: 9-point(TopLeading … BottomTrailing)"),

        h::h2("Lazy containers + Grid"),
        h::done_row("ScrollView + LazyVStack + LazyHStack"),
        h::sub("uniform-height/width 假设;variable-height = HostState cache,留 v2+"),
        h::done_row("Grid(uniform-cell m×n)+ grid_with_gaps(分 col_gap / row_gap)"),
        h::done_row("VariableGrid(Fixed / Flex / Auto tracks)"),
        h::done_row("lazy_vstack_padded(leading_margin, trailing_margin)"),
        h::sub("Lazy 容器加 top/bottom 滚动 padding;现 lazy_vstack 默认无 margin"),
        h::hint("    Grid 4 cols × 8 cells demo:"),
        grid(
            (0..8).map(|i| {
                let c = match i % 4 {
                    0 => color::ACCENT, 1 => color::SUCCESS,
                    2 => color::WARN,   _ => color::DANGER,
                };
                filled(c).corner_radius(radius::SM)
            }).collect(),
            4,
            Length::Pt(28.0),
            Length::Pt(28.0),
            Length::Pt(6.0),
        ),
        h::hint("    VariableGrid Fixed(20) Flex(1) Fixed(40) × 2 rows:"),
        crate::ui::view::variable_grid(
            (0..6).map(|i| {
                let c = match i % 3 {
                    0 => color::ACCENT, 1 => color::SUCCESS, _ => color::WARN,
                };
                filled(c).corner_radius(radius::SM)
            }).collect(),
            vec![
                crate::ui::view::GridTrack::Fixed(Length::Pt(20.0)),
                crate::ui::view::GridTrack::Flex(1),
                crate::ui::view::GridTrack::Fixed(Length::Pt(40.0)),
            ],
            vec![crate::ui::view::GridTrack::Fixed(Length::Pt(24.0))],
            Length::Pt(6.0),
            Length::Pt(6.0),
        ),

        h::h2("Gesture model"),
        h::done_row("InputEvent enum + hit_test_*"),
        h::sub("Click / DoubleClick / RightClick / DragBegin/Move/End / Hover / Scroll"),
        h::done_row("DragInProgress 状态机 + delta()"),
        h::sub("host 持 Option<DragInProgress>;Begin set / Move update / End drop"),
        h::done_row("ActionId / ScrollWheelId / DragId — elm-y reducer 派发"),

        h::h2("Stateful views"),
        h::done_row("Toggle / Picker(纯渲染)"),
        h::hint("    Toggle off / on(seeded HostState):"),
        hstack(vec![
            toggle(ViewId(0xDE7_1001)),
            toggle(ViewId(0xDE7_1002)),
        ]).hstack_gap(Length::Pt(20.0)).align_cross_center(),
        h::hint("    Picker(预选 \"Light\"):"),
        picker(ViewId(0xDE7_1003), vec!["Dark", "Light", "Auto"]),
        h::v2_row("TextField(NSTextInputClient + IME)"),
        h::sub("HostState API 已就绪;真接 AppKit IME 是单独大工程"),

        h::h2("Keyboard / Focus / Lifecycle modifiers"),
        h::done_row(".shortcut(KeyEquivalent, ActionId) — Cmd-key 等绑定"),
        h::sub("KeyEquivalent::{cmd(K), cmd_shift(K), ctrl(K), plain(K)} 构造器"),
        h::sub("App 层每帧扫 tree 收 shortcut 表;KeyDown 对照 → dispatch"),
        h::done_row(".focusable(FocusId) — 加入 Tab 导航环"),
        h::done_row(".auto_focus() — 首帧自动 focus"),
        h::done_row(".on_appear(ActionId) / .on_disappear(ActionId)"),
        h::sub("reconcile() diff 前后帧 live ids 生成 LifecycleEvent 列表"),
    ])
}

// ─── L5 Components(presets + 真组件)──────────────────────────

fn build_l5_view() -> crate::ui::view::View {
    use crate::ui::view::{
        Text, Edges, FrameSpec,
        vstack, hstack, filled, ActionId,
    };
    use crate::ui::theme::{color, space, radius, text, elev};
    use crate::ui::core::Length;

    h::scrollable_page(0xDE7_0050, vec![
        h::h2("Composable presets — 已落"),
        h::sub("Card / Panel / Badge / Tooltip / TabStrip — 由 modifier chain 组合"),

        h::h3("Card"),
        h::sub("background + border + corner_radius + shadow 组合"),
        crate::ui::view::card(
            vstack(vec![
                h::body("Card title"),
                h::hint("Card body content — bg_raised + border + radius + shadow E1"),
            ]).vstack_gap(Length::Pt(4.0)),
        ),

        h::h3("Panel"),
        h::sub("bg_panel + padding(MD)+ corner_radius MD"),
        crate::ui::view::panel(
            vstack(vec![
                h::body("Panel"),
                h::hint("背景灰一阶,不带 shadow.适合 sidebar / form section."),
            ]).vstack_gap(Length::Pt(4.0)),
        ),

        h::h3("Badge"),
        h::sub("pill-shape;color::* 区分语义状态"),
        hstack(vec![
            crate::ui::view::badge("New", color::ACCENT),
            crate::ui::view::badge("Done", color::SUCCESS),
            crate::ui::view::badge("Warn", color::WARN),
            crate::ui::view::badge("Fail", color::DANGER),
            crate::ui::view::badge("v2+", color::FG_MUTED),
        ]).hstack_gap(Length::Pt(10.0)).align_cross_center(),

        h::h3("Tooltip"),
        h::sub("v1 渲染样;真 hover-trigger 路径 = host 接 Hover event"),
        crate::ui::view::tooltip("This is a tooltip ↓"),

        h::h3("TabStrip"),
        h::sub("tab_strip(labels, selected, ActionId) — click 派发 reducer"),
        crate::ui::view::tab_strip(
            vec!["General", "Appearance", "Privacy", "Advanced"],
            1,
            ActionId(0xDE7_2000),
        ),

        h::h3("ContextMenu"),
        h::sub("context_menu(items, divider_after_idx, ActionId) — card + click row"),
        crate::ui::view::context_menu(
            vec!["Copy", "Paste", "Cut", "Select All", "Inspect"],
            Some(2),
            ActionId(0xDE7_2100),
        ).frame(crate::ui::view::FrameSpec {
            width: Some(Length::Pt(200.0)),
            ..Default::default()
        }),

        h::h3("Breadcrumb"),
        h::sub("breadcrumb(segments) — Home › Section › 最右 active 强色"),
        crate::ui::view::breadcrumb(vec!["Home", "Settings", "Appearance", "Theme"]),

        h::h3("List row"),
        h::sub("list_row(label, trailing, selected, ActionId) — sidebar/table row 通用"),
        vstack(vec![
            crate::ui::view::list_row("README.md",      Some("12 KB"),  false, ActionId(0xDE7_2200)),
            crate::ui::view::list_row("src/main.rs",    Some("4.2 KB"), true,  ActionId(0xDE7_2201)),
            crate::ui::view::list_row("docs/spec.md",   Some("8.7 KB"), false, ActionId(0xDE7_2202)),
            crate::ui::view::list_row("Cargo.toml",     Some("1.1 KB"), false, ActionId(0xDE7_2203)),
        ]).vstack_gap(Length::Pt(2.0)),

        h::h2("真组件迁移(P3i)"),
        h::v1_row("ContextMenu / Sidebar / Table — preset 已落,真组件迁移仍 v1 待补"),
        h::sub("preset = 可组合的 modifier 链 building block;真组件 = 替换 marspot 现存"),
        h::v1_row("LayoutModal / SearchOverlay / ProcessMonitor / DevPanel 主框架"),
        h::v1_row("ViewPainter 退役(P3j)— 全 component 迁完后顺手做"),
    ])
}

// ─── L6 Cross-cutting ─────────────────────────────────────────

fn build_l6_view() -> crate::ui::view::View {
    use crate::ui::view::{Text, FrameSpec, vstack, hstack, filled, AnimCurve, Anim};
    use crate::ui::theme::{color, radius, text, ThemeId};
    use crate::ui::core::Length;

    let easing_demo = |curve: AnimCurve, lab: &'static str| {
        // Sample the curve at t=0.5 and use that as a fill percentage
        // — visual cue of the easing shape.
        let mid: f64 = curve.ease(0.5);
        vstack(vec![
            filled(color::BG_PANEL).corner_radius(radius::SM).frame(FrameSpec {
                width: Some(Length::Pt(60.0)),
                height: Some(Length::Pt(8.0)),
                ..Default::default()
            }),
            filled(color::ACCENT).corner_radius(radius::SM).frame(FrameSpec {
                width: Some(Length::Pt((60.0 * mid).max(2.0))),
                height: Some(Length::Pt(8.0)),
                ..Default::default()
            }),
            h::hint(lab),
            h::hint(&format!("    mid t=0.5 → {:.2}", mid)),
        ]).vstack_gap(Length::Pt(2.0)).align_cross_start()
    };

    h::scrollable_page(0xDE7_0060, vec![
        h::h2("Lifecycle"),
        h::done_row("reconcile(&LaidOut) → Vec<LifecycleEvent>"),
        h::sub("每帧 build+layout 后调,diff prev/live ids 生成 Appear/Disappear 事件"),
        h::sub("Appear { id, action }/Disappear { id, action } — host 按 action 派发"),
        h::done_row(".on_appear(ActionId) / .on_disappear(ActionId) modifier"),
        h::sub("bake 进 Decoration.on_appear / on_disappear;reconcile() 跟踪 prev frame"),

        h::h2("Accessibility"),
        h::done_row(".accessibility_label(s) / .accessibility_role(AxRole)"),
        h::sub("bake 进 Decoration.ax_label / ax_role"),
        h::sub("AxRole: Button / Heading / ListItem / TextField / Image / StaticText /"),
        h::sub("        Group / Link / Checkbox / Toggle"),
        h::v2_row("真接 NSAccessibility(VoiceOver / 自动 AX tree)"),

        h::h2("Theme"),
        h::done_row("ThemeId { Dark, Light, HighContrast }"),
        h::sub("theme::current() / theme::set_current(id) — AtomicU8 全局"),
        h::done_row("Dark + Light token data 已 land(themed::color::* 闭包查 active)"),
        hstack(vec![
            h::hint("    ThemeId::Dark = "),
            filled(color::BG).corner_radius(radius::SM).frame(FrameSpec {
                width: Some(Length::Pt(48.0)), height: Some(Length::Pt(20.0)),
                ..Default::default()
            }),
            filled(color::ACCENT).corner_radius(radius::SM).frame(FrameSpec {
                width: Some(Length::Pt(48.0)), height: Some(Length::Pt(20.0)),
                ..Default::default()
            }),
            filled(color::FG).corner_radius(radius::SM).frame(FrameSpec {
                width: Some(Length::Pt(48.0)), height: Some(Length::Pt(20.0)),
                ..Default::default()
            }),
        ]).hstack_gap(Length::Pt(6.0)).align_cross_center(),
        hstack(vec![
            h::hint("    ThemeId::Light = "),
            filled(crate::ui::theme::color::light::BG).corner_radius(radius::SM).frame(FrameSpec {
                width: Some(Length::Pt(48.0)), height: Some(Length::Pt(20.0)),
                ..Default::default()
            }),
            filled(crate::ui::theme::color::light::ACCENT).corner_radius(radius::SM).frame(FrameSpec {
                width: Some(Length::Pt(48.0)), height: Some(Length::Pt(20.0)),
                ..Default::default()
            }),
            filled(crate::ui::theme::color::light::FG).corner_radius(radius::SM).frame(FrameSpec {
                width: Some(Length::Pt(48.0)), height: Some(Length::Pt(20.0)),
                ..Default::default()
            }),
        ]).hstack_gap(Length::Pt(6.0)).align_cross_center(),
        h::done_row("HighContrast palette(新增,跟 Light 同结构)"),
        hstack(vec![
            h::hint("    ThemeId::HighContrast = "),
            filled(crate::ui::theme::color::hc::BG).corner_radius(radius::SM).frame(FrameSpec {
                width: Some(Length::Pt(48.0)), height: Some(Length::Pt(20.0)),
                ..Default::default()
            }),
            filled(crate::ui::theme::color::hc::ACCENT).corner_radius(radius::SM).frame(FrameSpec {
                width: Some(Length::Pt(48.0)), height: Some(Length::Pt(20.0)),
                ..Default::default()
            }),
            filled(crate::ui::theme::color::hc::FG).corner_radius(radius::SM).frame(FrameSpec {
                width: Some(Length::Pt(48.0)), height: Some(Length::Pt(20.0)),
                ..Default::default()
            }),
        ]).hstack_gap(Length::Pt(6.0)).align_cross_center(),
        h::done_row("真 theme swap redraw hook(theme::version() AtomicU64 counter)"),
        h::sub("set_current(id) 自动 fetch_add;host redraw() 比对版本号 → request_redraw"),

        h::h2("Animation"),
        h::done_row("Anim<T> + Lerp + AnimCurve 5 curves(含 Spring)"),
        hstack(vec![
            easing_demo(AnimCurve::Linear,                  "Linear"),
            easing_demo(AnimCurve::EaseIn,                  "EaseIn"),
            easing_demo(AnimCurve::EaseOut,                 "EaseOut"),
            easing_demo(AnimCurve::EaseInOut,               "EaseInOut"),
            easing_demo(AnimCurve::Spring { bounce: 1.0 },  "Spring"),
        ]).hstack_gap(Length::Pt(14.0)).align_cross_start(),
        h::sub("Spring = damped-cosine 关闭式;真 ODE 弹簧 = v2+"),
        h::done_row("AnimRegistry + tick(now) + any_active() — frame schedule"),
        h::sub("anim_start(dur) 注册;anim_tick(Instant::now()) 推进;anim_any_active() 触发 request_redraw"),
        h::sub("ShellApp::redraw 每帧 tick+gc;active 时连续 redraw,idle 时 0(兼容 idle CPU=0)"),
        h::done_row(".transition(Transition) modifier — declarative enter/exit anim"),
        h::sub("Opacity / Scale / Slide(4 dir)/ Combined;LifecycleEvent driver 留 P3i 接绑"),

        h::h2("Internationalization"),
        h::done_row("text_width_cells 走 char_width(CJK = 2 cells)"),
        h::sub("Layout / paint / truncate 共用一张 East Asian Wide 表"),
        h::v2_row("RTL(Leading/Trailing 命名已留接口;实际 bidi 走 Unicode UAX#9)"),
        h::v2_row("Locale-aware 数字 / 日期 / pluralisation"),
    ])
}

/// Legacy `draw_model_sample` — kept around as a fallback in case
/// the View-tree path needs to be bypassed.  Not on the default
/// code path; remove once the new pipeline has soaked.
#[allow(dead_code)]
fn draw_model_sample(canvas: &mut Canvas, x: f64, mut y: f64) -> f64 {
    let line_h = 18.0;
    let block_gap = 14.0;

    // 1. Length
    canvas.text(Length::Pt(x), Length::Pt(y), "Length = Pt | Pct")
        .color(tokens::SECTION_BODY_FG)
        .draw();
    y += line_h;
    canvas.text(Length::Pt(x + 16.0), Length::Pt(y),
        "Pt(N) ≈ CSS N px — logical pt, scale-independent")
        .color(tokens::SAMPLE_HINT_FG)
        .draw();
    y += line_h;
    canvas.text(Length::Pt(x + 16.0), Length::Pt(y),
        "Pct(F) = F × parent (0.0..1.0)")
        .color(tokens::SAMPLE_HINT_FG)
        .draw();
    y += block_gap + line_h;

    // 2. Box model — Rect builder
    canvas.text(Length::Pt(x), Length::Pt(y), "Rect = box-model")
        .color(tokens::SECTION_BODY_FG)
        .draw();
    y += line_h;
    let api_lines = [
        ".at(x, y)              — top / left",
        ".size(w, h)            — width / height",
        ".fill(color)           — background",
        ".border(width, color)  — border (inside-stroke, box-sizing: border-box)",
        ".radius(r)             — border-radius",
        ".shadow(blur, off, c)  — box-shadow",
    ];
    for line in api_lines.iter() {
        canvas.text(Length::Pt(x + 16.0), Length::Pt(y), line)
            .color(tokens::SAMPLE_HINT_FG)
            .draw();
        y += line_h;
    }
    y += block_gap;

    // 3. Color — CSS rgba
    canvas.text(Length::Pt(x), Length::Pt(y), "Color = CSS rgba")
        .color(tokens::SECTION_BODY_FG)
        .draw();
    y += line_h;
    canvas.text(Length::Pt(x + 16.0), Length::Pt(y),
        "Color::rgba(r, g, b, a)  — r/g/b 0..255, a 0.0..1.0")
        .color(tokens::SAMPLE_HINT_FG)
        .draw();
    y += line_h;
    canvas.text(Length::Pt(x + 16.0), Length::Pt(y),
        "alpha 跟 CSS rgba() 一致;0.0 完全透明,1.0 不透明")
        .color(tokens::SAMPLE_HINT_FG)
        .draw();
    y += block_gap + line_h;

    // 4. Z-order — submission order
    canvas.text(Length::Pt(x), Length::Pt(y), "Z order = submission order")
        .color(tokens::SECTION_BODY_FG)
        .draw();
    y += line_h;
    canvas.text(Length::Pt(x + 16.0), Length::Pt(y),
        "later .draw() paints on top (no z-index, no explicit ordering)")
        .color(tokens::SAMPLE_HINT_FG)
        .draw();
    y += line_h + 6.0;

    // Z-order visual demo: 3 overlapping rects of decreasing size,
    // each .draw() lands above the previous.  Labels to the right.
    let demo_x = x + 16.0;
    let demo_w = 64.0;
    let demo_h = 32.0;
    let demo_step = 16.0;
    let z1 = Color::rgba(220,  60,  60, 1.0);  // red
    let z2 = Color::rgba( 80, 190, 110, 1.0);  // green
    let z3 = Color::rgba( 91, 162, 250, 1.0);  // blue
    // Draw in 1→2→3 order; expect green to cover red's right edge,
    // blue to cover green's right edge.
    canvas.rect()
        .at(Length::Pt(demo_x), Length::Pt(y))
        .size(Length::Pt(demo_w), Length::Pt(demo_h))
        .fill(z1)
        .radius(Pt(4.0))
        .draw();
    canvas.rect()
        .at(Length::Pt(demo_x + demo_step), Length::Pt(y + 6.0))
        .size(Length::Pt(demo_w), Length::Pt(demo_h))
        .fill(z2)
        .radius(Pt(4.0))
        .draw();
    canvas.rect()
        .at(Length::Pt(demo_x + demo_step * 2.0), Length::Pt(y + 12.0))
        .size(Length::Pt(demo_w), Length::Pt(demo_h))
        .fill(z3)
        .radius(Pt(4.0))
        .draw();
    canvas.text(
        Length::Pt(demo_x + demo_w + demo_step * 2.0 + 12.0),
        Length::Pt(y + 18.0),
        "1st .draw() ← red,  2nd ← green,  3rd ← blue (top)",
    )
    .color(tokens::SAMPLE_HINT_FG)
    .draw();
    y += demo_h + 20.0;

    y
}

/// Colors: a row of named swatches.  Demonstrates `Color::rgba` +
/// the named chrome tokens.  Each swatch is 48×32 with a 1pt border
/// and a hex-ish label below.
fn draw_colors_sample(canvas: &mut Canvas, x: f64, y: f64, cell_w_pt: f64) -> f64 {
    use crate::ui::core::Color;
    let swatches: &[(&str, Color)] = &[
        ("red",     Color::rgba(220,  60,  60, 1.0)),
        ("green",   Color::rgba( 80, 190, 110, 1.0)),
        ("blue",    Color::rgba( 91, 162, 250, 1.0)),
        ("yellow",  Color::rgba(230, 200,  80, 1.0)),
        ("purple",  Color::rgba(160, 100, 220, 1.0)),
        ("cyan",    Color::rgba( 80, 200, 220, 1.0)),
        ("alpha50", Color::rgba(255, 255, 255, 0.50)),
    ];
    let sw_w = 56.0;
    let sw_h = 28.0;
    let gap = 8.0;
    for (i, (name, c)) in swatches.iter().enumerate() {
        let sx = x + (i as f64) * (sw_w + gap);
        canvas.rect()
            .at(Length::Pt(sx), Length::Pt(y))
            .size(Length::Pt(sw_w), Length::Pt(sw_h))
            .fill(*c)
            .radius(Pt(3.0))
            .border(Pt(1.0), tokens::DIVIDER)
            .draw();
        // Truncate label to swatch width so it doesn't overflow.
        let max_chars = (sw_w / cell_w_pt).floor() as usize;
        let trimmed: String = name.chars().take(max_chars.max(3)).collect();
        canvas.text(Length::Pt(sx), Length::Pt(y + sw_h + 4.0), &trimmed)
            .color(tokens::SAMPLE_HINT_FG)
            .draw();
    }
    y + sw_h + 22.0
}

/// Units: horizontal bars sized in Pt vs Pct so the user can see how
/// `Length` resolves.  Three Pt rules + two Pct rules; labels right of
/// each bar identify which `Length::*` produced it.
fn draw_units_sample(canvas: &mut Canvas, x: f64, y: f64) -> f64 {
    let entries: &[(&str, Length, Color)] = &[
        ("Pt(40)",   Length::Pt(40.0),  Color::rgba(91, 162, 250, 1.0)),
        ("Pt(80)",   Length::Pt(80.0),  Color::rgba(91, 162, 250, 1.0)),
        ("Pt(160)",  Length::Pt(160.0), Color::rgba(91, 162, 250, 1.0)),
        ("Pct(25)",  Length::Pct(0.25), Color::rgba(160, 100, 220, 1.0)),
        ("Pct(50)",  Length::Pct(0.50), Color::rgba(160, 100, 220, 1.0)),
    ];
    let bar_h = 14.0;
    let row_h = 22.0;
    for (i, (label, len, color)) in entries.iter().enumerate() {
        let row_y = y + (i as f64) * row_h;
        // Bars are drawn relative to a sub-canvas would be cleaner;
        // for now Pct resolves against the WHOLE window which is
        // wider than the content column.  That's fine — the user
        // can visually compare absolute and relative sizes.
        canvas.rect()
            .at(Length::Pt(x), Length::Pt(row_y))
            .size(*len, Length::Pt(bar_h))
            .fill(*color)
            .radius(Pt(2.0))
            .draw();
        canvas.text(Length::Pt(x + 220.0), Length::Pt(row_y + 1.0), label)
            .color(tokens::SAMPLE_HINT_FG)
            .draw();
    }
    y + (entries.len() as f64) * row_h
}

/// Rects: variants of the rect builder — solid fill, border only,
/// radius, shadow.  Each shown as a 60×40 box with a hint label below.
fn draw_rects_sample(canvas: &mut Canvas, x: f64, y: f64) -> f64 {
    let box_w = 64.0;
    let box_h = 40.0;
    let gap = 16.0;
    let label_y_off = box_h + 4.0;
    let fill_color = Color::rgba(91, 162, 250, 1.0);

    // 1. Solid fill.
    canvas.rect()
        .at(Length::Pt(x), Length::Pt(y))
        .size(Length::Pt(box_w), Length::Pt(box_h))
        .fill(fill_color)
        .draw();
    canvas.text(Length::Pt(x), Length::Pt(y + label_y_off), "fill")
        .color(tokens::SAMPLE_HINT_FG)
        .draw();

    // 2. Border only (transparent fill).
    let x2 = x + (box_w + gap);
    canvas.rect()
        .at(Length::Pt(x2), Length::Pt(y))
        .size(Length::Pt(box_w), Length::Pt(box_h))
        .fill(Color::rgba(0, 0, 0, 0.0))
        .border(Pt(1.0), fill_color)
        .draw();
    canvas.text(Length::Pt(x2), Length::Pt(y + label_y_off), "border")
        .color(tokens::SAMPLE_HINT_FG)
        .draw();

    // 3. Rounded corners.
    let x3 = x + 2.0 * (box_w + gap);
    canvas.rect()
        .at(Length::Pt(x3), Length::Pt(y))
        .size(Length::Pt(box_w), Length::Pt(box_h))
        .fill(fill_color)
        .radius(Pt(10.0))
        .draw();
    canvas.text(Length::Pt(x3), Length::Pt(y + label_y_off), "radius(10)")
        .color(tokens::SAMPLE_HINT_FG)
        .draw();

    // 4. Shadow.
    let x4 = x + 3.0 * (box_w + gap);
    canvas.rect()
        .at(Length::Pt(x4), Length::Pt(y))
        .size(Length::Pt(box_w), Length::Pt(box_h))
        .fill(fill_color)
        .radius(Pt(6.0))
        .shadow(Pt(10.0), (Pt(0.0), Pt(2.0)), Color::rgba(0, 0, 0, 0.6))
        .draw();
    canvas.text(Length::Pt(x4), Length::Pt(y + label_y_off), "shadow")
        .color(tokens::SAMPLE_HINT_FG)
        .draw();

    y + label_y_off + 16.0
}

/// Lines: width / color variants of the line builder, plus a couple
/// of orientations.  Shown in a horizontal strip 4pt apart so the
/// user can compare line weights.
fn draw_lines_sample(canvas: &mut Canvas, x: f64, y: f64) -> f64 {
    let row_h = 14.0;
    let entries: &[(&str, f64, Color, f64)] = &[
        ("1pt", 1.0, Color::rgba(91, 162, 250, 1.0), 0.0),
        ("2pt", 2.0, Color::rgba(91, 162, 250, 1.0), row_h),
        ("3pt", 3.0, Color::rgba(91, 162, 250, 1.0), row_h * 2.0),
        ("alpha", 1.0, Color::rgba(255, 255, 255, 0.30), row_h * 3.0),
    ];
    for (label, w, color, dy) in entries.iter() {
        let row_y = y + *dy + 6.0;
        canvas.line(
            (Length::Pt(x), Length::Pt(row_y)),
            (Length::Pt(x + 200.0), Length::Pt(row_y)),
        )
        .stroke(Pt(*w), *color)
        .draw();
        canvas.text(Length::Pt(x + 220.0), Length::Pt(row_y - 5.0), label)
            .color(tokens::SAMPLE_HINT_FG)
            .draw();
    }
    // A diagonal accent — uncommon usage; shows that `line` isn't
    // axis-locked.
    canvas.line(
        (Length::Pt(x), Length::Pt(y + row_h * 4.0 + 4.0)),
        (Length::Pt(x + 200.0), Length::Pt(y + row_h * 4.0 + 18.0)),
    )
    .stroke(Pt(1.0), Color::rgba(91, 162, 250, 0.85))
    .draw();
    canvas.text(Length::Pt(x + 220.0), Length::Pt(y + row_h * 4.0 + 6.0), "diagonal")
        .color(tokens::SAMPLE_HINT_FG)
        .draw();
    y + row_h * 4.0 + 24.0
}

/// Text: same `Text` primitive, different colors / contents.  Demonstrates
/// the only `text()` knobs we have (position + color); size is fixed by the
/// renderer's chrome font cell.
fn draw_text_sample(canvas: &mut Canvas, x: f64, y: f64) -> f64 {
    let row_h = 20.0;
    let entries: &[(&str, Color)] = &[
        ("Default — SECTION_BODY_FG (rgba 200 208 220)", tokens::SECTION_BODY_FG),
        ("Hint — SAMPLE_HINT_FG (rgba 130 140 156)",      tokens::SAMPLE_HINT_FG),
        ("Accent — TAB_ACTIVE_ACCENT (blue 91 162 250)",  tokens::TAB_ACTIVE_ACCENT),
        ("Header — SECTION_HEADER_FG (light blue)",       tokens::SECTION_HEADER_FG),
        ("Red — explicit Color::rgba(220, 60, 60)",       Color::rgba(220, 60, 60, 1.0)),
    ];
    for (i, (content, color)) in entries.iter().enumerate() {
        canvas.text(Length::Pt(x), Length::Pt(y + (i as f64) * row_h), content)
            .color(*color)
            .draw();
    }
    y + (entries.len() as f64) * row_h
}

/// Phase 10c — Font v5 showcase rebuilt as a view tree.  Layout
/// queries `FontMetricsProvider::advance_phys` for every SF Pro
/// run, so column gaps + line heights track the real font metrics
/// instead of hand-tuned Pt magic numbers.  Returns a `View` that
/// `render_view_section` lays out + paints into the dev-panel
/// content area at whatever width the panel is sized to.
fn build_font_v5_view() -> crate::ui::view::View {
    use crate::ui::view::{vstack, hstack, Text};
    use crate::ui::core::Length;
    use crate::font_shape::ShapeOptions;
    use crate::ui::theme::{color, text as text_tok};

    // SF Pro at chrome scale.  13pt matches `FontCache::UI_FONT_POINT`
    // so the showcase is laid out against the same font instance the
    // renderer will paint with — no chance of layout / paint width
    // mismatch.
    const SF: f64 = 13.0;
    let full = ShapeOptions::full();
    let off = ShapeOptions::all_off();

    let header = |s: &str| Text::new(s).style(text_tok::HEADER).ui(SF, 600, full).build();
    let hint = |s: &str| Text::new(s).style(text_tok::HINT).ui(SF, 400, full).build();
    let body = |s: &str| Text::new(s).color(color::FG).ui(SF, 400, full).build();
    let body_w = |s: &str, w: u16| Text::new(s).color(color::FG).ui(SF, w, full).build();
    let body_off = |s: &str| Text::new(s).color(color::FG).ui(SF, 400, off).build();

    let kv = |label: &str, content: crate::ui::view::View| -> crate::ui::view::View {
        hstack(vec![hint(label), content]).hstack_gap(Length::Pt(8.0))
    };

    // Labels kept short so headers fit the ~250-pt content area at
    // default panel width.  Long demo strings (subpx, kerning, etc.)
    // intentionally exceed and rely on the truncate path to clip
    // gracefully — visible "…" reads as "longer than panel".
    vstack(vec![
        // ── Phase 5 — variable weight ─────────────────────────
        header("P5 weight"),
        hstack(vec![
            body_w("Thin", 100),
            body_w("Light", 300),
            body_w("Regular", 400),
        ]).hstack_gap(Length::Pt(12.0)),
        hstack(vec![
            body_w("Semibold", 600),
            body_w("Bold", 700),
            body_w("Black", 900),
        ]).hstack_gap(Length::Pt(12.0)),

        // ── Phase 8 — ligatures default vs all_off ────────────
        header("P8 liga: default vs all_off"),
        kv("on:", body("fi fl ffi ->")),
        kv("off:", body_off("fi fl ffi ->")),

        // ── Phase 3 — kerning + proportional advance ──────────
        header("P3 kerning"),
        kv("kerned:", body("Ta AV LT WA")),
        kv("raw:", body_off("Ta AV LT WA")),

        // ── Phase 7 — chrome colour emoji ─────────────────────
        header("P7 colour emoji"),
        body("👍 🚀 🎉 ❤️ 🌈 ⭐ 🍎"),

        // ── Phase 3 — CJK auto-fallback ───────────────────────
        header("P3 CJK fallback"),
        body("Hello 你好 こんにちは 안녕"),

        // ── Phase 4 — subpixel x positioning ──────────────────
        header("P4 sub-pixel x"),
        body("iiiiii lllll AVAVAV"),
    ]).vstack_gap(Length::Pt(12.0))
}

/// Font v5 showcase — each Phase's headline capability rendered side
/// by side so the user can SEE the difference between off and on.
/// Every text run inside this section calls `.ui()` to opt INTO SF
/// Pro proportional shape, so the rest of the dev panel (which laid
/// itself out against Monaco mono cell metrics) keeps its original
/// look unaffected by the showcase.
fn draw_font_v5_sample(canvas: &mut Canvas, x: f64, y: f64) -> f64 {
    use crate::font_shape::ShapeOptions;

    let fg = tokens::SECTION_BODY_FG;
    let hint = tokens::SAMPLE_HINT_FG;
    let row_h = 22.0;
    let label_w = 96.0;
    let mut cursor_y = y;

    // ─── Phase 5 — variable font weights ─────────────────────
    canvas.text(Length::Pt(x), Length::Pt(cursor_y), "Phase 5 — variable weight")
        .color(tokens::SECTION_HEADER_FG)
        .ui()
        .draw();
    cursor_y += row_h;
    let weights: &[(u16, &str)] = &[
        (100, "Thin 100"),
        (300, "Light 300"),
        (400, "Regular 400"),
        (600, "Semibold 600"),
        (700, "Bold 700"),
        (900, "Black 900"),
    ];
    let mut col_x = x;
    for (w, label) in weights {
        canvas.text(Length::Pt(col_x), Length::Pt(cursor_y), label)
            .color(fg)
            .weight(*w)
            .ui()
            .draw();
        col_x += 105.0;
    }
    cursor_y += row_h * 1.4;

    // ─── Phase 8 — ShapeOptions liga off vs default ──────────
    canvas.text(Length::Pt(x), Length::Pt(cursor_y), "Phase 8 — ligatures (CTLine default vs all_off)")
        .color(tokens::SECTION_HEADER_FG)
        .ui()
        .draw();
    cursor_y += row_h;
    canvas.text(Length::Pt(x), Length::Pt(cursor_y), "default:")
        .color(hint)
        .ui()
        .draw();
    canvas.text(Length::Pt(x + label_w), Length::Pt(cursor_y), "fi fl ffi -> => >= !=")
        .color(fg)
        .ui()
        .draw();
    cursor_y += row_h;
    canvas.text(Length::Pt(x), Length::Pt(cursor_y), "all_off:")
        .color(hint)
        .ui()
        .draw();
    canvas.text(Length::Pt(x + label_w), Length::Pt(cursor_y), "fi fl ffi -> => >= !=")
        .color(fg)
        .opts(ShapeOptions::all_off())
        .ui()
        .draw();
    cursor_y += row_h * 1.4;

    // ─── Phase 3 — kerning (Ta AV LT) at the chrome size ─────
    canvas.text(Length::Pt(x), Length::Pt(cursor_y), "Phase 3 — kerning + proportional advance")
        .color(tokens::SECTION_HEADER_FG)
        .ui()
        .draw();
    cursor_y += row_h;
    canvas.text(Length::Pt(x), Length::Pt(cursor_y), "kerned:")
        .color(hint)
        .ui()
        .draw();
    canvas.text(Length::Pt(x + label_w), Length::Pt(cursor_y), "Ta AV LT WA — Yes")
        .color(fg)
        .ui()
        .draw();
    cursor_y += row_h;
    // CT auto-kerning still runs even with `kerning=false` in
    // ShapeOptions (Phase 8 left that toggle as TODO), but liga
    // off + the integer-position pen shows visibly looser spacing.
    canvas.text(Length::Pt(x), Length::Pt(cursor_y), "raw shape:")
        .color(hint)
        .ui()
        .draw();
    canvas.text(Length::Pt(x + label_w), Length::Pt(cursor_y), "Ta AV LT WA — Yes")
        .color(fg)
        .opts(ShapeOptions::all_off())
        .ui()
        .draw();
    cursor_y += row_h * 1.4;

    // ─── Phase 7 — chrome colour emoji ───────────────────────
    canvas.text(Length::Pt(x), Length::Pt(cursor_y), "Phase 7 — chrome colour emoji")
        .color(tokens::SECTION_HEADER_FG)
        .ui()
        .draw();
    cursor_y += row_h;
    canvas.text(Length::Pt(x), Length::Pt(cursor_y), "👍 🚀 🎉 ❤️ 🌈 ⭐ 🍎 🐙 🍣")
        .color(fg)
        .ui()
        .draw();
    cursor_y += row_h * 1.4;

    // ─── Phase 3 — automatic CJK fallback ────────────────────
    canvas.text(Length::Pt(x), Length::Pt(cursor_y), "Phase 3 — CJK auto-fallback (SF Pro → PingFang / Hiragino)")
        .color(tokens::SECTION_HEADER_FG)
        .ui()
        .draw();
    cursor_y += row_h;
    canvas.text(Length::Pt(x), Length::Pt(cursor_y), "Hello 你好世界 こんにちは 안녕하세요")
        .color(fg)
        .ui()
        .draw();
    cursor_y += row_h * 1.4;

    // ─── Phase 4 — subpixel positioning at small sizes ───────
    canvas.text(Length::Pt(x), Length::Pt(cursor_y), "Phase 4 — sub-pixel x (0.25-px buckets)")
        .color(tokens::SECTION_HEADER_FG)
        .ui()
        .draw();
    cursor_y += row_h;
    // Repeated narrow glyphs — the pre-Phase-4 chrome path would
    // black-clump the column of `i`s; Phase 4 yields visible spacing.
    canvas.text(Length::Pt(x), Length::Pt(cursor_y), "iiiiiiiiiiiiii  lllllllllll  mmmmmm  AVAVAV  WAWAWA")
        .color(fg)
        .ui()
        .draw();
    cursor_y += row_h;

    cursor_y
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::core::Primitive;

    #[test]
    fn default_state_is_visible_and_centred_ish() {
        let s = DevPanelState::default();
        assert!(s.visible);
        assert_eq!(s.active_tab, TAB_UI);
        assert_eq!(s.active_section, SECTION_MODEL);
        assert!(s.size_pt.0 > 200.0);
        assert!(s.size_pt.1 > 200.0);
    }

    #[test]
    fn ui_tab_canvas_emits_bg_tab_strip_and_samples() {
        let s = DevPanelState::default();
        let fonts = crate::ui::view::MockFontMetrics { cell_w_phys: 16.0, cell_h_phys: 32.0 };
        let c = build_dev_panel_canvas(&s, 1920.0, 1080.0, 16.0, 32.0, 24.0, &fonts);
        let prims = c.primitives();
        // Substantial output — BG, tab strip BG, active tab + accent,
        // tab labels, hairline, menu BG, divider, 5 menu rows + 1
        // active highlight, "Colors" section header + divider, 7
        // swatches + 7 labels.  Conservative lower bound guards
        // against catastrophic regression to the placeholder.
        assert!(prims.len() > 20, "got {} primitives, expected > 20", prims.len());
        // First primitive must be the full-window BG rect (so the dev
        // window isn't a transparent slit).
        match &prims[0] {
            Primitive::Rect(_) => (),
            _ => panic!("expected BG rect first"),
        }
    }

    #[test]
    fn non_ui_tab_renders_placeholder() {
        let s = DevPanelState { active_tab: TAB_TOKENS, ..Default::default() };
        let fonts = crate::ui::view::MockFontMetrics { cell_w_phys: 8.0, cell_h_phys: 16.0 };
        let c = build_dev_panel_canvas(&s, 800.0, 600.0, 8.0, 16.0, 12.0, &fonts);
        // Just BG + tab strip BG + active tab BG + accent + a few tab
        // labels + divider hairline + placeholder text.  Don't pin
        // the exact count (it shifts as tab list grows) — just check
        // we got something coherent and the placeholder text is in
        // there.
        let prims = c.primitives();
        let has_placeholder = prims.iter().any(|p| matches!(p, Primitive::Text(t) if t.content.contains("placeholder")));
        assert!(has_placeholder, "expected placeholder text on non-UI tab");
    }

    #[test]
    fn hit_test_lands_on_tab_strip() {
        let s = DevPanelState::default();
        // y inside the tab strip; x at left edge → first tab "UI".
        // chrome_cell_w_pt=8 matches the test renderer dim.
        let h = hit_test(&s, 8.0, 5.0, 10.0).expect("expected hit");
        assert_eq!(h, DevPanelHit::Tab(TAB_UI));
        // Click further right — past UI tab → Tokens.
        // UI width = 2 chars * 8 + 32 = 48 ; Tokens starts at x=48.
        let h2 = hit_test(&s, 8.0, 60.0, 10.0).expect("expected hit");
        assert_eq!(h2, DevPanelHit::Tab(TAB_TOKENS));
    }

    #[test]
    fn hit_test_lands_on_menu_row() {
        let s = DevPanelState::default();
        // y just past the tab strip + menu top pad → first row "Model".
        let h = hit_test(&s, 8.0, 20.0, TAB_BAR_H_PT + MENU_TOP_PAD_PT + 5.0)
            .expect("expected hit");
        assert_eq!(h, DevPanelHit::Section(SECTION_MODEL));
        // Two rows down → 3rd row.  Post 0.6.21 restructure the
        // menu reads:  Model / L1 / L2 / L3 / L4 / L5 / L6 / Colors /
        // Units / ...  so idx 2 = SECTION_L2 (was SECTION_UNITS).
        let h2 = hit_test(
            &s, 8.0, 20.0,
            TAB_BAR_H_PT + MENU_TOP_PAD_PT + 2.0 * MENU_ROW_H_PT + 5.0,
        ).expect("expected hit");
        assert_eq!(h2, DevPanelHit::Section(SECTION_L2));
    }

    #[test]
    fn hit_test_returns_none_in_content_area() {
        let s = DevPanelState::default();
        // x past menu_w, y past tab strip — no actionable region here.
        assert_eq!(hit_test(&s, 8.0, MENU_W_PT + 50.0, TAB_BAR_H_PT + 100.0), None);
    }

    #[test]
    fn hit_test_skips_menu_when_not_on_ui_tab() {
        // Even if y is in the menu row band, hit-test must not return
        // a Section when active_tab != UI (other tabs don't render menu).
        let s = DevPanelState { active_tab: TAB_TOKENS, ..Default::default() };
        assert_eq!(hit_test(&s, 8.0, 20.0, TAB_BAR_H_PT + 30.0), None);
    }

    #[test]
    fn hidden_state_still_builds_a_canvas() {
        // The renderer guards on `state.visible` before calling, so
        // an invisible state never reaches this function.  But the
        // builder mustn't panic when called — defensive lower bound
        // for the input space.
        let s = DevPanelState { visible: false, ..Default::default() };
        let fonts = crate::ui::view::MockFontMetrics { cell_w_phys: 8.0, cell_h_phys: 16.0 };
        let c = build_dev_panel_canvas(&s, 800.0, 600.0, 8.0, 16.0, 12.0, &fonts);
        assert!(c.len() > 0);
    }
}
