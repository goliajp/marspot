//! `Button` — clickable rectangle with optional label + optional icon.
//! Supports four real layouts via `IconPosition`:
//!
//!   - `Only`    icon-only(no text), icon centered
//!   - `Before`  [icon] gap text     (icon left, text right)
//!   - `After`   text gap [icon]     (icon right, text left)
//!   - Pure text  (icon = None, label = Some) — handled automatically
//!
//! Hovered state is a separate visual; caller flips `hovered` based
//! on its own hit-test cycle.
//!
//! Custom-shape icons (e.g. the toolbar's sidebar / grid / list-tree
//! glyphs are tiny stroked rects, not characters) plug in via
//! `IconSpec::Custom(&dyn Fn(p, icon_rect))` — the closure paints
//! whatever it wants into the icon box.

use marspot_term::layout::Rect;
use crate::ui::core::{ViewPainter, IconComponent};

#[derive(Debug, Clone, Copy)]
pub struct ButtonStyle {
    pub bg: [f32; 4],
    pub bg_hover: [f32; 4],
    pub fg: [f32; 4],
    pub fg_hover: [f32; 4],
    pub border_color: [f32; 4],
    pub border_width: f32,
    pub corner_radius: f32,
    /// Horizontal inset inside the button rect — text + icon group
    /// is centered, but never crosses these margins.
    pub padding_x: f32,
    /// Gap between icon and text (in icon-then-text / text-then-icon
    /// layouts).
    pub icon_gap: f32,
    /// Square slot reserved for the icon (in both Custom and Glyph
    /// modes).  Tunes how big the icon "looks" inside the button.
    pub icon_size: f32,
}

impl Default for ButtonStyle {
    fn default() -> Self {
        Self {
            bg:           [0.18, 0.19, 0.23, 1.0],
            bg_hover:     [0.26, 0.28, 0.34, 1.0],
            fg:           [0.92, 0.94, 0.97, 1.0],
            fg_hover:     [1.00, 1.00, 1.00, 1.0],
            border_color: [0.30, 0.32, 0.38, 1.0],
            border_width: 1.0,
            corner_radius: 6.0,
            padding_x: 8.0,
            icon_gap: 6.0,
            icon_size: 12.0,
        }
    }
}

impl ButtonStyle {
    /// Ghost button — no fill / no border at idle; subtle fill on
    /// hover.  Used for icon-only controls sitting INSIDE an already-
    /// opaque parent surface (search bar's × / Aa, toolbar icons,
    /// inline chrome).  The "no transparent overlay" invariant
    /// applies to `View` chrome (outer surface vs grid), not to
    /// buttons stacked on an already-opaque parent.
    pub fn ghost() -> Self {
        Self {
            bg:           [0.0, 0.0, 0.0, 0.0],
            bg_hover:     [1.0, 1.0, 1.0, 0.08],
            fg:           [0.60, 0.63, 0.70, 1.0],
            fg_hover:     [1.00, 1.00, 1.00, 1.0],
            border_color: [0.0, 0.0, 0.0, 0.0],
            border_width: 0.0,
            corner_radius: 4.0,
            padding_x: 4.0,
            icon_gap: 4.0,
            icon_size: 12.0,
        }
    }

    /// Toolbar chrome — quiet raised surface on idle, slightly deeper
    /// on hover.  Pairs with the chrome icons (sidebar / grid /
    /// list-tree) to make the toolbar buttons visually one family.
    pub fn chrome() -> Self {
        Self {
            bg:           [0.085, 0.095, 0.115, 1.0],
            bg_hover:     [0.130, 0.140, 0.165, 1.0],
            fg:           [0.55, 0.60, 0.65, 1.0],
            fg_hover:     [0.75, 0.80, 0.85, 1.0],
            border_color: [0.18, 0.20, 0.23, 1.0],
            border_width: 1.0,
            corner_radius: 4.0,
            padding_x: 4.0,
            icon_gap: 4.0,
            icon_size: 14.0,
        }
    }

    /// Destructive / kill button — red-tinted variant for "you sure?"
    /// affordances like the process panel row [×] or sidebar close-
    /// session [×].  Same shape as the default filled button, just
    /// red fills.
    pub fn destructive() -> Self {
        Self {
            bg:           [0.30, 0.10, 0.11, 1.0],
            bg_hover:     [0.45, 0.16, 0.18, 1.0],
            fg:           [0.96, 0.70, 0.70, 1.0],
            fg_hover:     [1.00, 0.85, 0.85, 1.0],
            border_color: [0.0, 0.0, 0.0, 0.0],
            border_width: 0.0,
            corner_radius: 4.0,
            padding_x: 4.0,
            icon_gap: 4.0,
            icon_size: 12.0,
        }
    }
}

#[derive(Copy, Clone)]
pub enum IconSpec<'a> {
    /// Render a short string (typically one char) via the glyph
    /// atlas.  Centered in the icon slot.  Use for "×", "+", "▸",
    /// text-like glyphs that already live in the font.
    Glyph(&'a str),
    /// A named, tested `IconComponent` impl (e.g. `GridIcon`,
    /// `SidebarIcon`, `ListTreeIcon`).  No raw paint closures in
    /// scene code — pick a concrete icon from
    /// `system/macos/icons/*` or `components/icons/*`, or add a new
    /// one there with its own unit test.
    Component(&'a dyn IconComponent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconPosition {
    /// Icon centered; label (if any) ignored.
    Only,
    /// [icon] gap text  — icon to the left of text.
    Before,
    /// text gap [icon]  — icon to the right of text.
    After,
}

pub struct Button<'a> {
    pub rect: Rect,
    pub label: Option<&'a str>,
    pub icon: Option<IconSpec<'a>>,
    pub icon_position: IconPosition,
    pub hovered: bool,
    pub style: ButtonStyle,
}

impl<'a> Button<'a> {
    pub fn hit_test(&self, x: f64, y: f64) -> bool {
        self.rect.contains(x, y)
    }

    pub fn paint(&self, p: &mut ViewPainter) {
        // Background (rounded rect via UI pipeline).  Skipped when BG
        // alpha and border are both 0 — that's the "ghost button at
        // idle" path, no draw needed.  Saves a UI rect per chrome
        // button per frame.
        let bg = if self.hovered { self.style.bg_hover } else { self.style.bg };
        let fg = if self.hovered { self.style.fg_hover } else { self.style.fg };
        if bg[3] > 0.0 || self.style.border_width > 0.0 {
            p.fill_rounded_rect(
                self.rect,
                bg,
                self.style.corner_radius,
                (self.style.border_color, self.style.border_width),
            );
        }

        // Layout for label + icon group, horizontally centered with
        // padding_x respected on both sides.
        let cell_w = p.cell_w;
        let cell_h = p.cell_h;
        let ascent = p.ascent;
        let inner_x = self.rect.x as f32 + self.style.padding_x;
        let inner_right = (self.rect.x + self.rect.w) as f32 - self.style.padding_x;
        let inner_w = (inner_right - inner_x).max(0.0);
        let cy = self.rect.y_top as f32 + (self.rect.h as f32) * 0.5;
        let icon_size = self.style.icon_size.min(self.rect.h as f32 - 2.0).max(0.0);

        match (&self.icon, self.icon_position, self.label) {
            (Some(icon), IconPosition::Only, _) | (Some(icon), _, None) => {
                // Icon only (either explicitly Only, or no label provided).
                let ix = self.rect.x as f32
                    + (self.rect.w as f32 - icon_size) * 0.5;
                let iy = cy - icon_size * 0.5;
                let icon_rect = Rect {
                    x: ix as f64,
                    y_top: iy as f64,
                    w: icon_size as f64,
                    h: icon_size as f64,
                };
                paint_icon(p, icon, icon_rect, fg, cell_w, cell_h, ascent);
            }
            (None, _, Some(label)) => {
                // Text only.
                let label_chars = label.chars().count() as f32;
                let label_w = (label_chars * cell_w).min(inner_w);
                let label_x = inner_x + (inner_w - label_w) * 0.5;
                let baseline = cy - cell_h * 0.5 + ascent;
                p.text(label_x, baseline, label, fg);
            }
            (Some(icon), pos, Some(label)) => {
                let label_chars = label.chars().count() as f32;
                let label_w = label_chars * cell_w;
                let group_w = (icon_size + self.style.icon_gap + label_w).min(inner_w);
                let group_x = inner_x + (inner_w - group_w) * 0.5;
                let (icon_x, label_x) = match pos {
                    IconPosition::Before => {
                        let ix = group_x;
                        let lx = ix + icon_size + self.style.icon_gap;
                        (ix, lx)
                    }
                    IconPosition::After => {
                        let lx = group_x;
                        let ix = lx + label_w + self.style.icon_gap;
                        (ix, lx)
                    }
                    IconPosition::Only => {
                        // Shouldn't reach (matched above), but defensively
                        // fall through to centered icon.
                        let ix = self.rect.x as f32
                            + (self.rect.w as f32 - icon_size) * 0.5;
                        (ix, ix)
                    }
                };
                let icon_rect = Rect {
                    x: icon_x as f64,
                    y_top: (cy - icon_size * 0.5) as f64,
                    w: icon_size as f64,
                    h: icon_size as f64,
                };
                paint_icon(p, icon, icon_rect, fg, cell_w, cell_h, ascent);
                let baseline = cy - cell_h * 0.5 + ascent;
                p.text(label_x, baseline, label, fg);
            }
            (None, _, None) => {
                // Bare button — just the BG fill.  Caller probably has
                // a reason (placeholder while loading?).
            }
        }
    }
}

fn paint_icon(
    p: &mut ViewPainter,
    icon: &IconSpec<'_>,
    rect: Rect,
    fg: [f32; 4],
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
) {
    match icon {
        IconSpec::Glyph(s) => {
            // Center the glyph inside `rect`.  Glyph width = cell_w
            // per char; height = cell_h.
            let chars = s.chars().count() as f32;
            let text_w = chars * cell_w;
            let tx = rect.x as f32 + (rect.w as f32 - text_w) * 0.5;
            let ty = rect.y_top as f32 + (rect.h as f32 - cell_h) * 0.5 + ascent;
            p.text(tx, ty, s, fg);
        }
        IconSpec::Component(c) => c.paint(p, rect, fg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y_top: y, w, h }
    }

    #[test]
    fn hit_test_inside_outside() {
        let b = Button {
            rect: rect(10.0, 20.0, 100.0, 30.0),
            label: Some("ok"),
            icon: None,
            icon_position: IconPosition::Only,
            hovered: false,
            style: ButtonStyle::default(),
        };
        assert!(b.hit_test(50.0, 30.0));
        assert!(!b.hit_test(5.0, 30.0));
        assert!(!b.hit_test(115.0, 30.0));
        assert!(!b.hit_test(50.0, 60.0));
    }

    #[test]
    fn default_style_is_opaque() {
        let s = ButtonStyle::default();
        assert_eq!(s.bg[3], 1.0, "button BG must be opaque");
        assert_eq!(s.fg[3], 1.0);
        assert_eq!(s.bg_hover[3], 1.0);
        assert_eq!(s.fg_hover[3], 1.0);
    }

    #[test]
    fn icon_position_variants_are_distinct() {
        assert_ne!(IconPosition::Only,   IconPosition::Before);
        assert_ne!(IconPosition::Before, IconPosition::After);
        assert_ne!(IconPosition::After,  IconPosition::Only);
    }

    #[test]
    fn ghost_style_has_zero_alpha_bg_idle() {
        let s = ButtonStyle::ghost();
        assert_eq!(s.bg[3], 0.0, "ghost button idle BG must be invisible");
        assert!(s.bg_hover[3] > 0.0, "hover BG must be visible to read as interactive");
        assert_eq!(s.fg[3], 1.0);
        assert_eq!(s.border_width, 0.0, "ghost has no border at idle");
    }

    #[test]
    fn destructive_style_uses_red_tones() {
        let s = ButtonStyle::destructive();
        assert!(s.bg[0] > s.bg[1] && s.bg[0] > s.bg[2],
            "destructive BG must be red-dominant");
        assert!(s.fg[0] > s.fg[1] && s.fg[0] > s.fg[2],
            "destructive FG must be red-dominant");
        assert_eq!(s.bg[3], 1.0);
        assert_eq!(s.fg[3], 1.0);
    }
}
