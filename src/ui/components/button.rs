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
//! An icon is a system symbol from the chrome's one table
//! (`system::macos::icons`), drawn at `style.icon_size`.

use marspot_term::layout::Rect;
use crate::ui::core::ViewPainter;
use crate::ui::system::macos::icons::{self, Symbol};
use crate::ui::components::panel_palette as palette;
use crate::ui::theme::spec::{chrome, space};
use crate::ui::theme::PanelText;

/// Every button label in the app, one role.  A button says a word to
/// the user; it is the same kind of text as a row's label, and it is
/// set the same size wherever the button is.
const BUTTON_LABEL: PanelText = PanelText::Label;

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
    /// The icon's point size, physical px, and the side of the slot
    /// kept for it beside a label.
    pub icon_size: f32,
}

impl ButtonStyle {
    /// Ghost button — nothing at rest but its glyph; a fill under the
    /// pointer.  For icon-only controls on a surface that is already
    /// opaque: the search overlay's × and `Aa`.
    pub fn ghost() -> Self {
        Self {
            bg: palette::none(),
            bg_hover: palette::chrome_hover(),
            fg: palette::chrome_icon(),
            fg_hover: palette::fg(),
            border_color: palette::none(),
            border_width: 0.0,
            corner_radius: chrome::px(chrome::TOOLBAR_RADIUS) as f32,
            padding_x: chrome::px(space::S4) as f32,
            icon_gap: chrome::px(space::S4) as f32,
            icon_size: chrome::px(chrome::TOOLBAR_ICON) as f32,
        }
    }

    /// A toolbar button: a ghost at rest, and the accent's tint while
    /// what it opens is open (`on`) — the same fill as the sidebar row
    /// of the pane in use.
    pub fn toolbar(on: bool) -> Self {
        let ghost = Self::ghost();
        if !on {
            return ghost;
        }
        Self {
            bg: palette::chrome_on(),
            bg_hover: palette::hover(palette::chrome_on()),
            fg: palette::fg(),
            ..ghost
        }
    }
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
    pub icon: Option<Symbol>,
    pub icon_position: IconPosition,
    pub hovered: bool,
    pub style: ButtonStyle,
}

impl<'a> Button<'a> {
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
        let inner_x = self.rect.x as f32 + self.style.padding_x;
        let inner_right = (self.rect.x + self.rect.w) as f32 - self.style.padding_x;
        let inner_w = (inner_right - inner_x).max(0.0);
        let cy = self.rect.y_top as f32 + (self.rect.h as f32) * 0.5;
        let icon_size = self.style.icon_size.min(self.rect.h as f32 - chrome::px(2.0) as f32).max(0.0);

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
                icons::paint(p, *icon, icon_rect, icon_size as f64, fg);
            }
            (None, _, Some(label)) => {
                // Text only.  The label is prose — a word the user
                // reads, not a column of figures — so it takes the
                // shared panel role, measured rather than counted.
                // Counting characters and multiplying by the mono cell
                // (what this did) leaves a proportional label
                // off-centre by however much the two disagree.
                let label_w = p.panel_text_width(BUTTON_LABEL, label).min(inner_w);
                let label_x = inner_x + (inner_w - label_w) * 0.5;
                let baseline = p.ui_baseline_centred_for(
                    label, self.rect.y_top as f32, self.rect.h as f32, BUTTON_LABEL.pt(), BUTTON_LABEL.weight(),
                );
                p.panel_text(BUTTON_LABEL, label_x, baseline, label, fg);
            }
            (Some(icon), pos, Some(label)) => {
                let label_w = p.panel_text_width(BUTTON_LABEL, label);
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
                icons::paint(p, *icon, icon_rect, icon_size as f64, fg);
                let baseline = p.ui_baseline_centred_for(
                    label, self.rect.y_top as f32, self.rect.h as f32, BUTTON_LABEL.pt(), BUTTON_LABEL.weight(),
                );
                p.panel_text(BUTTON_LABEL, label_x, baseline, label, fg);
            }
            (None, _, None) => {
                // Bare button — just the BG fill.  Caller probably has
                // a reason (placeholder while loading?).
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn toolbar_style_differs_from_a_ghost_only_when_on() {
        let (off, on, ghost) = (ButtonStyle::toolbar(false), ButtonStyle::toolbar(true), ButtonStyle::ghost());
        assert_eq!((off.bg, off.bg_hover, off.fg), (ghost.bg, ghost.bg_hover, ghost.fg));
        assert!(on.bg[3] > 0.0, "an on button is filled at rest");
        assert_ne!(on.bg, on.bg_hover, "and still answers the pointer");
        assert_eq!(on.corner_radius, off.corner_radius);
    }
}
