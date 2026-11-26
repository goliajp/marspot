//! A notice over the top of a pane: a framed card of terminal text,
//! wrapped at word boundaries to the pane's width.
//!
//! Framed and opaque like the search overlay, so what the pane shows
//! under it never reads through the words.  Sizes come from the cell
//! (already in physical pixels) and `chrome::px`, so it draws the same
//! at every display scale.

use super::panel_palette as palette;
use super::Panel;
use crate::pane_message::width;
use crate::ui::core::{Backdrop, ViewPainter, ViewStyle};
use crate::ui::theme::spec::{self, chrome, panel};
use marspot_term::layout::Rect;

/// The most lines drawn; what does not fit is cut.
pub const MAX_LINES: usize = 6;

/// `text` broken into lines of at most `cols` cells, at spaces where it
/// can be and inside a word only where a word is longer than a line.
pub fn wrap(text: &str, cols: u16) -> Vec<String> {
    let cols = cols.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let need = if line.is_empty() { width(word) } else { width(&line) + 1 + width(word) };
        if need <= cols {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        for ch in word.chars() {
            if width(&line) + width(ch.encode_utf8(&mut [0; 4])) > cols {
                lines.push(std::mem::take(&mut line));
            }
            line.push(ch);
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines.truncate(MAX_LINES);
    lines
}

pub struct PaneNoticeParams<'a> {
    pub text: &'a str,
    pub inner_x: f32,
    pub inner_y: f32,
    pub grid_cols: u16,
    pub grid_rows: u16,
}

pub fn paint_pane_notice(p: &mut ViewPainter, params: PaneNoticeParams<'_>) {
    if params.grid_cols < 12 || params.grid_rows < 3 {
        return;
    }
    let (cell_w, cell_h) = (p.cell_w, p.cell_h);
    let room = params.grid_cols - 4;
    let lines = wrap(params.text, room);
    let widest = lines.iter().map(|l| width(l)).max().unwrap_or(0);
    let pad = (cell_h * 0.4).max(chrome::px(chrome::SEARCH_PAD_MIN) as f32);
    let rows = (lines.len() as f32).min(params.grid_rows as f32 - 1.0);
    let rect = Rect {
        x: (params.inner_x + cell_w) as f64,
        y_top: (params.inner_y + spec::snap((cell_h * 0.5) as f64) as f32) as f64,
        w: (widest as f32 * cell_w + 2.0 * pad) as f64,
        h: (spec::snap((rows * cell_h) as f64) as f32 + 2.0 * pad) as f64,
    };
    let card = Panel::new(
        rect,
        ViewStyle {
            bg: palette::frame_bg(),
            border_color: palette::frame_border(),
            border_width: panel::BORDER_PX,
            corner_radius: chrome::px(chrome::POPOVER_RADIUS) as f32,
            shadow_blur: chrome::px(chrome::POPOVER_SHADOW_BLUR) as f32,
            shadow_alpha: panel::SHADOW_ALPHA,
            backdrop: Backdrop::None,
            padding: 0.0,
        },
        pad as f64,
    );
    card.paint(p, |p| {
        let content = card.content_rect();
        for (i, line) in lines.iter().take(rows as usize).enumerate() {
            let baseline = content.y_top as f32 + i as f32 * cell_h + p.ascent;
            p.text(content.x as f32, baseline, line, palette::fg());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notice_wraps_at_spaces_and_breaks_only_a_word_longer_than_a_line() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("  spaced   out  ", 20), ["spaced out"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("全部の アカウント", 6), ["全部の", "アカウ", "ント"]);
        assert_eq!(wrap(&"w ".repeat(40), 3).len(), MAX_LINES);
        assert!(wrap("", 10).is_empty());
    }
}
