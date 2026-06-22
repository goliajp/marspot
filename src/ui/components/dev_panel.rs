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
pub const MENU_W_PT: f64 = 140.0;
pub const TAB_PAD_X_PT: f64 = 16.0;
pub const TAB_TEXT_Y_PT: f64 = 11.0;
pub const MENU_ROW_H_PT: f64 = 28.0;
pub const MENU_TEXT_PAD_X_PT: f64 = 14.0;
pub const MENU_TOP_PAD_PT: f64 = 8.0;
pub const CONTENT_X_PAD_PT: f64 = 20.0;
pub const SECTION_GAP_PT: f64 = 24.0;

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
pub const SECTION_COLORS: usize = 1;
pub const SECTION_UNITS: usize = 2;
pub const SECTION_RECTS: usize = 3;
pub const SECTION_LINES: usize = 4;
pub const SECTION_TEXT: usize = 5;
const SECTION_LABELS: &[(&str, usize)] = &[
    ("Model", SECTION_MODEL),
    ("Colors", SECTION_COLORS),
    ("Units", SECTION_UNITS),
    ("Rects", SECTION_RECTS),
    ("Lines", SECTION_LINES),
    ("Text", SECTION_TEXT),
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

    match state.active_section {
        SECTION_MODEL => {
            let y = draw_section_header(&mut canvas, content_x, y, "Model");
            let _ = draw_model_sample(&mut canvas, content_x, y);
        }
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

/// Model: the CSS-like mental model the rest of the UI tab demos
/// piece by piece.  This is the "read me first" section — `Length`,
/// the box model, `Color`, z-order.  Mostly text + one Z-order
/// visual demo because z-order isn't shown elsewhere.
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
        let c = build_dev_panel_canvas(&s, 1920.0, 1080.0, 16.0, 32.0);
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
        let c = build_dev_panel_canvas(&s, 800.0, 600.0, 8.0, 16.0);
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
        // Two rows down → 3rd row = "Units".
        let h2 = hit_test(
            &s, 8.0, 20.0,
            TAB_BAR_H_PT + MENU_TOP_PAD_PT + 2.0 * MENU_ROW_H_PT + 5.0,
        ).expect("expected hit");
        assert_eq!(h2, DevPanelHit::Section(SECTION_UNITS));
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
        let c = build_dev_panel_canvas(&s, 800.0, 600.0, 8.0, 16.0);
        assert!(c.len() > 0);
    }
}
