//! A notice over the top of a pane: a framed card of terminal text,
//! wrapped at word boundaries to the pane's width, each line of the
//! text a paragraph of its own.  A question carries two buttons under
//! its text, the one that goes ahead first.
//!
//! Framed and opaque like the search overlay, so what the pane shows
//! under it never reads through the words.  Sizes come from the cell
//! (already in physical pixels) and `chrome::px`, so it draws the same
//! at every display scale.  Where it stands is worked out once, by
//! [`layout`], for the picture and for a press alike.

use super::controls::{self, ButtonKind};
use super::panel_palette as palette;
use super::Panel;
use crate::pane_message::width;
use crate::ui::core::{Backdrop, ViewPainter, ViewStyle};
use crate::ui::theme::spec::{self, chrome, control, panel};
use marspot_term::layout::Rect;

/// The most lines drawn; what does not fit is cut.
pub const MAX_LINES: usize = 6;
/// Between a question's text and its buttons' labels, and between the
/// two labels.
const BUTTONS_MARK: char = '\u{1e}';
const BUTTON_SEP: char = '\u{1f}';

/// The text of a question: `body`, then a button that goes ahead and
/// one that does not.
pub fn ask_text(body: &str, go: &str, stay: &str) -> String {
    format!("{body}{BUTTONS_MARK}{go}{BUTTON_SEP}{stay}")
}

/// `text` broken into lines of at most `cols` cells, each line of it
/// started afresh, at spaces where it can be and inside a word only
/// where a word is longer than a line.
pub fn wrap(text: &str, cols: u16) -> Vec<String> {
    let cols = cols.max(1);
    let mut lines: Vec<String> = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        for word in para.split_whitespace() {
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
    }
    lines.truncate(MAX_LINES);
    lines
}

pub struct PaneNoticeParams<'a> {
    pub text: &'a str,
    /// The button the pointer is on: 0 goes ahead, 1 does not.
    pub hover: Option<u8>,
    pub inner_x: f32,
    pub inner_y: f32,
    pub grid_cols: u16,
    pub grid_rows: u16,
}

/// Where a notice stands over its pane.
pub struct NoticeLayout<'a> {
    pub card: Rect,
    pad: f64,
    pub lines: Vec<String>,
    rows: usize,
    /// The button that goes ahead and the one that does not, each with
    /// its label.
    pub buttons: Option<[(Rect, &'a str); 2]>,
}

/// How wide a button is for `label`: its label's estimated width and
/// padding, never under the spec's least width.
fn button_w(label: &str) -> f64 {
    let ch = control::BUTTON_TEXT.pt() * 0.55 * ViewPainter::px_per_pt();
    let text = crate::ui::core::text_fit::cells(label) as f64 * ch;
    (text + 2.0 * spec::px(control::BUTTON_PAD_X)).max(spec::px(control::BUTTON_MIN_W))
}

/// Where the notice of `params` stands with a cell `cell` wide and
/// high; none in a pane too small to hold one.
pub fn layout<'a>(params: &PaneNoticeParams<'a>, (cell_w, cell_h): (f32, f32)) -> Option<NoticeLayout<'a>> {
    if params.grid_cols < 12 || params.grid_rows < 3 {
        return None;
    }
    let (body, asked) = match params.text.split_once(BUTTONS_MARK) {
        Some((body, buttons)) => (body, buttons.split_once(BUTTON_SEP)),
        None => (params.text, None),
    };
    let lines = wrap(body, params.grid_cols - 4);
    let widest = lines.iter().map(|l| width(l)).max().unwrap_or(0) as f64 * cell_w as f64;
    let pad = (cell_h * 0.4).max(chrome::px(chrome::SEARCH_PAD_MIN) as f32) as f64;
    let rows = lines.len().min(params.grid_rows as usize - 1);
    let gap = spec::px(spec::space::S8);
    let (button_h, buttons_w) = match asked {
        Some((go, stay)) => (spec::px(control::BUTTON_H), button_w(go) + gap + button_w(stay)),
        None => (0.0, 0.0),
    };
    let text_h = spec::snap(rows as f64 * cell_h as f64);
    let card = Rect {
        x: params.inner_x as f64 + cell_w as f64,
        y_top: params.inner_y as f64 + spec::snap(cell_h as f64 * 0.5),
        w: widest.max(buttons_w) + 2.0 * pad,
        h: text_h + 2.0 * pad + if asked.is_some() { gap + button_h } else { 0.0 },
    };
    let buttons = asked.map(|(go, stay)| {
        let y_top = card.y_top + pad + text_h + gap;
        let go_rect = Rect { x: card.x + pad, y_top, w: button_w(go), h: button_h };
        let stay_rect = Rect { x: go_rect.x + go_rect.w + gap, y_top, w: button_w(stay), h: button_h };
        [(go_rect, go), (stay_rect, stay)]
    });
    Some(NoticeLayout { card, pad, lines, rows, buttons })
}

pub fn paint_pane_notice(p: &mut ViewPainter, params: PaneNoticeParams<'_>) {
    let Some(l) = layout(&params, (p.cell_w, p.cell_h)) else { return };
    let card = Panel::new(
        l.card,
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
        l.pad,
    );
    card.paint(p, |p| {
        let content = card.content_rect();
        for (i, line) in l.lines.iter().take(l.rows).enumerate() {
            let baseline = content.y_top as f32 + i as f32 * p.cell_h + p.ascent;
            p.text(content.x as f32, baseline, line, palette::fg());
        }
        for (i, (r, label)) in l.buttons.iter().flatten().enumerate() {
            let kind = if i == 0 { ButtonKind::Primary } else { ButtonKind::Secondary };
            controls::paint_button(p, *r, label, kind, params.hover == Some(i as u8), false);
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
        assert_eq!(wrap("first line\nsecond", 40), ["first line", "second"], "each line starts afresh");
    }

    #[test]
    fn a_question_has_its_buttons_under_its_text_inside_the_card() {
        let text = ask_text("Closing this pane also ends these child panes:\n#8590 madpdf", "Close All", "Cancel");
        let params = PaneNoticeParams { text: &text, hover: None, inner_x: 100.0, inner_y: 50.0, grid_cols: 80, grid_rows: 24 };
        let l = layout(&params, (8.0, 16.0)).expect("room for it");
        assert_eq!(l.lines, ["Closing this pane also ends these child panes:", "#8590 madpdf"], "the labels are no text");
        let [(go, go_label), (stay, stay_label)] = l.buttons.expect("two buttons");
        assert_eq!((go_label, stay_label), ("Close All", "Cancel"));
        assert!(go.y_top >= l.card.y_top + 2.0 * 16.0, "under the text");
        assert!(stay.x > go.x + go.w, "side by side, the one that goes ahead first");
        let inside = |r: Rect| r.x >= l.card.x && r.x + r.w <= l.card.x + l.card.w && r.y_top + r.h <= l.card.y_top + l.card.h;
        assert!(inside(go) && inside(stay), "{:?} {go:?} {stay:?}", l.card);
        let plain = PaneNoticeParams { text: "just words", ..params };
        assert!(layout(&plain, (8.0, 16.0)).expect("room").buttons.is_none());
    }
}
