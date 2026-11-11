//! The keyboard shortcuts panel: where everything in it goes.
//!
//! Every command that has a chord, grouped by the menu it is in, the
//! chord in key caps at the right edge of its row.  The rows are the
//! command table's (`commands::reference`); this module only places
//! them.  Two columns of groups, three menus each, so the panel is
//! about as tall as the others rather than twice that.
//!
//! No painter: [`walk`] says where each piece is, and
//! `shortcuts_paint` draws what it is told.  All rects are physical
//! pixels.

use marspot_term::layout::Rect;

use crate::commands::{self, Keys, Menu};
use crate::ui::components::controls;
use crate::ui::components::panel_chrome::{self, PanelChrome};
use crate::ui::strings::Msg;
use crate::ui::theme::spec::{self, card, control, space};

pub mod metric {
    use crate::ui::theme::{spec::space, PanelText};

    /// Panel width: two columns, each wide enough for the longest
    /// command name and the widest pair of key caps on one row.
    pub const PANEL_W: f64 = 640.0;
    /// Between the two columns of groups.
    pub const COLUMN_GAP: f64 = space::S24;
    /// A row's inset above and below its key caps.
    pub const ROW_PAD_Y: f64 = space::S6;
    /// A command's name: a list entry, not a label over an explanation.
    pub const TITLE: PanelText = PanelText::Item;
}

/// The menus down each column, in menu-bar order read column by column.
pub const COLUMNS: [[Menu; 3]; 2] =
    [[Menu::App, Menu::File, Menu::Edit], [Menu::View, Menu::Window, Menu::Help]];

/// One piece of a row's keys, right-aligned in the card.
#[derive(Debug, Clone, PartialEq)]
pub struct Cap {
    pub rect: Rect,
    pub label: String,
    /// False for the dash between the two ends of a range, which is
    /// set like a cap's glyphs but has no cap behind it.
    pub filled: bool,
}

/// A piece of the panel, with where it goes, in draw order.
#[derive(Debug, Clone, PartialEq)]
pub enum Slot {
    /// A group's heading: its line starts at `top`, over the card's
    /// width `w` from its left edge `x`.
    Group { heading: Msg, x: f64, top: f64, w: f64 },
    /// A group's card.  Emitted before the rows it contains.
    Card { rect: Rect },
    /// Hairline between two rows of `card`, its top edge at `y`.
    Separator { card: Rect, y: f64 },
    Row { title: Msg, title_x: f64, title_baseline: f64, caps: Vec<Cap> },
}

/// A row's height for a terminal cell `cell_h` tall: its key caps and
/// the padding around them.
fn row_h(cell_h: f64) -> f64 {
    controls::keycap_size("", 0.0, cell_h).1 + 2.0 * spec::px(metric::ROW_PAD_Y)
}

/// The caps of `keys`, the last one ending at `right`, all `top` down.
fn caps(keys: Keys, right: f64, top: f64, cell: (f64, f64)) -> Vec<Cap> {
    let cap = |c: commands::Chord| (c.to_string(), true);
    let parts = match keys {
        Keys::One(a) => vec![cap(a)],
        Keys::Either(a, b) => vec![cap(a), cap(b)],
        Keys::Range(a, b) => vec![cap(a), ("–".to_string(), false), cap(b)],
    };
    // Two caps stand a gap apart.  The dash is as wide as a cap would
    // be, and that padding is all the room it needs from its caps.
    let gap = spec::px(control::KEYCAP_GAP);
    let mut x = right;
    let mut out: Vec<Cap> = Vec::with_capacity(parts.len());
    for (label, filled) in parts.into_iter().rev() {
        if out.last().is_some_and(|after: &Cap| after.filled && filled) {
            x -= gap;
        }
        let (w, h) = controls::keycap_size(&label, cell.0, cell.1);
        x -= w;
        out.push(Cap { rect: Rect { x, y_top: top, w, h }, label, filled });
    }
    out.reverse();
    out
}

/// Walks the panel's content in draw order.  One walker for the
/// painter and for the height, so the frame is as tall as what is
/// drawn in it.
///
/// `rect` is the panel's frame and `cell` the terminal font's cell,
/// which the key caps are set in.  A row that does not fit whole above
/// the content area's bottom edge is left out, and a card is cut
/// there: the painter does not clip.
pub fn walk(rect: Rect, cell: (f64, f64), mut on: impl FnMut(Slot)) {
    let content = PanelChrome::new(rect, 0.0).content;
    let bottom = content.y_top + content.h + 1e-9;
    let gap = spec::px(metric::COLUMN_GAP);
    let col_w = spec::snap_down((content.w - gap) * 0.5).max(0.0);
    let row_h = row_h(cell.1);
    let pad_x = spec::px(card::ROW_PAD_X);
    let rows = commands::reference();

    for (ci, menus) in COLUMNS.iter().enumerate() {
        let x = content.x + ci as f64 * (col_w + gap);
        let mut y = content.y_top;
        for (gi, menu) in menus.iter().enumerate() {
            if gi > 0 {
                y += spec::px(space::CARD_TO_HEADING);
            }
            let in_menu = rows.iter().filter(|r| r.menu == *menu);
            let card_top = y + panel_chrome::heading_height();
            let full =
                Rect { x, y_top: card_top, w: col_w, h: in_menu.clone().count() as f64 * row_h };
            if card_top + row_h > bottom {
                break;
            }
            on(Slot::Group { heading: menu.title(), x, top: y, w: col_w });
            on(Slot::Card { rect: Rect { h: full.h.min(bottom - card_top), ..full } });
            for (ri, r) in in_menu.enumerate() {
                let top = card_top + ri as f64 * row_h;
                if top + row_h > bottom {
                    break;
                }
                if ri > 0 {
                    on(Slot::Separator { card: full, y: top });
                }
                on(Slot::Row {
                    title: r.title,
                    title_x: x + pad_x,
                    title_baseline: top + (row_h + spec::px(metric::TITLE.cap())) * 0.5,
                    caps: caps(r.keys, x + col_w - pad_x, top + spec::px(metric::ROW_PAD_Y), cell),
                });
            }
            y = card_top + full.h;
        }
    }
}

/// Height of everything [`walk`] emits when nothing is cut.
pub fn content_h(cell: (f64, f64)) -> f64 {
    let unbounded = Rect { x: 0.0, y_top: 0.0, w: spec::px(metric::PANEL_W), h: f64::INFINITY };
    let top = PanelChrome::new(unbounded, 0.0).content.y_top;
    let mut bottom = top;
    walk(unbounded, cell, |slot| {
        if let Slot::Card { rect } = slot {
            bottom = bottom.max(rect.y_top + rect.h);
        }
    });
    bottom - top
}

/// The panel's frame, centred, as tall as its rows.
pub fn panel_rect(window_w: f64, window_h: f64, cell: (f64, f64), top_inset: f64) -> Rect {
    panel_chrome::frame_rect(
        window_w,
        window_h,
        spec::px(metric::PANEL_W),
        panel_chrome::frame_height(content_h(cell)),
        top_inset,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::strings::t;

    const CELL: (f64, f64) = (17.0, 34.0);

    fn frame() -> Rect {
        panel_rect(2400.0, 1600.0, CELL, 0.0)
    }

    fn slots(rect: Rect) -> Vec<Slot> {
        let mut out = Vec::new();
        walk(rect, CELL, |s| out.push(s));
        out
    }

    fn rows(slots: &[Slot]) -> Vec<(Msg, &[Cap])> {
        slots
            .iter()
            .filter_map(|s| match s {
                Slot::Row { title, caps, .. } => Some((*title, caps.as_slice())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn shortcuts_panel_lists_every_chord_of_the_table_once() {
        let slots = slots(frame());
        let shown: Vec<(Msg, String)> = rows(&slots)
            .iter()
            .map(|(title, caps)| {
                (*title, caps.iter().map(|c| c.label.as_str()).collect::<Vec<_>>().join(" "))
            })
            .collect();
        // The table's rows in column order are the panel's rows.
        let want: Vec<(Msg, String)> = COLUMNS
            .iter()
            .flatten()
            .flat_map(|menu| commands::reference().into_iter().filter(move |r| r.menu == *menu))
            .map(|r| (r.title, r.keys.to_string().replace(", ", " ")))
            .collect();
        assert_eq!(shown, want);
        assert_eq!(shown.len(), commands::reference().len());
        assert!(shown.contains(&(Msg::CmdClosePanel, "⌘W Esc".to_string())));
        assert!(shown.contains(&(Msg::CmdPaneRange, "⌘1 – ⌘9".to_string())));
        let headings: Vec<&str> = slots
            .iter()
            .filter_map(|s| match s {
                Slot::Group { heading, .. } => Some(t(*heading)),
                _ => None,
            })
            .collect();
        assert_eq!(headings, ["Marspot", "File", "Edit", "View", "Window", "Help"]);
    }

    #[test]
    fn shortcuts_panel_key_caps_end_on_one_edge_per_column() {
        let slots = slots(frame());
        let cards: Vec<Rect> = slots
            .iter()
            .filter_map(|s| match s {
                Slot::Card { rect } => Some(*rect),
                _ => None,
            })
            .collect();
        assert_eq!(cards.len(), 6);
        let pad = spec::px(card::ROW_PAD_X);
        let mut edges: Vec<f64> = Vec::new();
        for (title, caps) in rows(&slots) {
            let last = caps.last().expect("a row has keys");
            let right = last.rect.x + last.rect.w;
            let card = cards
                .iter()
                .find(|c| c.contains(last.rect.x, last.rect.y_top))
                .unwrap_or_else(|| panic!("{title:?}: its keys are on no card"));
            assert_eq!(right, card.x + card.w - pad, "{title:?}");
            // Whole inside the card, top to bottom and left to right.
            for cap in caps {
                assert!(cap.rect.x >= card.x + pad, "{title:?} runs into the name's side");
                assert!(cap.rect.y_top + cap.rect.h <= card.y_top + card.h, "{title:?}");
            }
            // Left to right: a gap between two caps, none beside the
            // dash of a range.
            for pair in caps.windows(2) {
                let gap = pair[1].rect.x - (pair[0].rect.x + pair[0].rect.w);
                let want = if pair[0].filled && pair[1].filled { spec::px(control::KEYCAP_GAP) } else { 0.0 };
                assert!((gap - want).abs() < 1e-9, "{title:?}");
            }
            if !edges.contains(&right) {
                edges.push(right);
            }
        }
        assert_eq!(edges.len(), 2, "one right edge per column");
    }

    #[test]
    fn shortcuts_panel_columns_sit_side_by_side_inside_the_content() {
        let rect = frame();
        let content = PanelChrome::new(rect, 0.0).content;
        let mut by_x: Vec<(f64, f64)> = Vec::new();
        let mut lowest: f64 = 0.0;
        for slot in slots(rect) {
            if let Slot::Card { rect: c } = slot {
                assert!(c.x >= content.x && c.x + c.w <= content.x + content.w + 1e-9);
                assert!(c.y_top >= content.y_top);
                lowest = lowest.max(c.y_top + c.h);
                if !by_x.contains(&(c.x, c.w)) {
                    by_x.push((c.x, c.w));
                }
            }
        }
        let [(x0, w0), (x1, w1)] = by_x[..] else { panic!("{by_x:?}") };
        assert_eq!(w0, w1);
        assert_eq!(x1 - (x0 + w0), spec::px(metric::COLUMN_GAP));
        // The frame wraps the taller column, to the pixel it was
        // rounded up to.
        assert!((lowest - (content.y_top + content.h)).abs() < 1.0, "{lowest} vs {content:?}");
    }

    #[test]
    fn shortcuts_panel_range_has_a_bare_dash_between_two_caps() {
        let slots = slots(frame());
        let all = rows(&slots);
        let (_, range) = all.iter().find(|(t, _)| *t == Msg::CmdPaneRange).unwrap();
        assert_eq!(range.iter().map(|c| c.filled).collect::<Vec<_>>(), [true, false, true]);
        let (_, either) = all.iter().find(|(t, _)| *t == Msg::CmdClosePanel).unwrap();
        assert_eq!(either.iter().map(|c| c.filled).collect::<Vec<_>>(), [true, true]);
    }

    /// The painter does not clip, so in a window too short for the
    /// panel nothing may be placed below the content area.
    #[test]
    fn shortcuts_panel_leaves_out_what_a_short_window_cannot_show() {
        let rect = panel_rect(2400.0, 500.0, CELL, 0.0);
        let content = PanelChrome::new(rect, 0.0).content;
        let bottom = content.y_top + content.h + 1e-6;
        let slots = slots(rect);
        assert!(rows(&slots).len() < commands::reference().len(), "the window is too short for all");
        assert!(!rows(&slots).is_empty());
        for slot in &slots {
            match slot {
                Slot::Card { rect } => assert!(rect.y_top + rect.h <= bottom),
                Slot::Separator { y, .. } => assert!(*y <= bottom),
                Slot::Group { top, .. } => assert!(top + panel_chrome::heading_height() <= bottom),
                Slot::Row { caps, .. } => {
                    assert!(caps.iter().all(|c| c.rect.y_top + c.rect.h <= bottom))
                }
            }
        }
    }
}
