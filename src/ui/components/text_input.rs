//! `TextInput` — single-line text field.  Renders into a fixed rect:
//! value text (left-aligned, baseline-centered) + a thin accent caret
//! at the char position.  Truncates with no ellipsis when the rect
//! can't hold the whole value.
//!
//! No input handling here — that's the caller's job (`mouse_down`/
//! `key`).  This component is paint-only.
//!
//! Use case: search bar query, edit-mode pane title, future "rename
//! tab" prompt.

use marspot_term::layout::Rect;
use crate::ui::core::ViewPainter;

#[derive(Debug, Clone, Copy)]
pub struct TextInputStyle {
    pub fg: [f32; 4],
    pub caret_color: [f32; 4],
    /// Caret stripe width in physical px.
    pub caret_w: f32,
    /// Vertical inset for the caret stripe (so it doesn't span the
    /// whole row height — looks crisper at ~2 px top + bottom).
    pub caret_inset_y: f32,
}

impl Default for TextInputStyle {
    fn default() -> Self {
        Self {
            fg: [0.95, 0.96, 0.97, 1.0],
            caret_color: [0.40, 0.62, 1.00, 0.95],
            caret_w: 2.0,
            caret_inset_y: 2.0,
        }
    }
}

pub struct TextInput<'a> {
    pub rect: Rect,
    pub value: &'a str,
    /// Char position of the caret inside `value`.  Clamped to
    /// `value.chars().count()`.
    pub cursor: u16,
    pub style: TextInputStyle,
}

impl<'a> TextInput<'a> {
    pub fn paint(&self, p: &mut ViewPainter) {
        let cell_w = p.cell_w;
        let cell_h = p.cell_h;
        let ascent = p.ascent;
        // Truncate to what fits the rect at cell_w; no ellipsis since
        // the caret usually sits near typed-end so the user sees what
        // they're typing.
        let max_chars = (self.rect.w as f32 / cell_w).floor() as usize;
        let display: String = self.value.chars().take(max_chars).collect();
        let baseline = self.rect.y_top as f32 + (self.rect.h as f32 - cell_h) * 0.5 + ascent;
        if !display.is_empty() {
            p.text(self.rect.x as f32, baseline, &display, self.style.fg);
        }
        // Caret.
        let caret_col = (self.cursor as usize).min(max_chars) as f32;
        let caret_x = self.rect.x as f32 + caret_col * cell_w;
        let caret_y = self.rect.y_top as f32 + self.style.caret_inset_y;
        let caret_h = (self.rect.h as f32 - 2.0 * self.style.caret_inset_y).max(2.0);
        p.fill_rect(
            Rect {
                x: caret_x as f64,
                y_top: caret_y as f64,
                w: self.style.caret_w as f64,
                h: caret_h as f64,
            },
            self.style.caret_color,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_style_is_visible() {
        let s = TextInputStyle::default();
        assert!(s.caret_w > 0.0);
        assert!(s.caret_inset_y >= 0.0);
        assert_eq!(s.fg[3], 1.0, "input text must be opaque");
    }
}
