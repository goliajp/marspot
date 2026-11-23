//! The `+N` after a pane's title: how many shadow panes the pane's
//! session has.  Plain text in the title's own cells, and clickable.
//!
//! The name gives way to it, as it does to the badge.

use std::hash::{Hash, Hasher};

use crate::ui::components::sidebar::clip_chars;
use crate::ui::core::text_fit::cells;
use crate::ui::theme::spec::chrome;

/// Where a strip's name ends and its `+N` is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Fit {
    /// Characters of the name that are drawn.
    pub name_chars: usize,
    /// The `+N`, as `(x, w)`; None with nothing to say or no room.
    pub mark: Option<(f64, f64)>,
}

/// How many characters `+n` is.
fn mark_cells(n: u32) -> usize {
    2 + n.checked_ilog10().unwrap_or(0) as usize
}

/// Fit `title` and the mark for `n` in the strip from `x`, `w` wide,
/// before `badge`.
pub(crate) fn fit((x, w): (f64, f64), padding: f64, title: &str, badge: &str, n: u32, cell_w: f64) -> Fit {
    let label_x = x + padding;
    let edge = x + w - padding;
    let badge_x = edge - cells(badge) as f64 * cell_w;
    let right = if !badge.is_empty() && badge_x > label_x { badge_x - chrome::px(chrome::TITLE_GAP) } else { edge };
    let room = ((right - label_x) / cell_w).floor().max(0.0) as usize;
    let wanted = mark_cells(n) + 1;
    if n == 0 || room < wanted {
        return Fit { name_chars: room, mark: None };
    }
    let name_room = room - wanted;
    let name = cells(clip_chars(title, name_room)).min(name_room);
    let mark_x = label_x + (name + 1) as f64 * cell_w;
    Fit { name_chars: name_room, mark: Some((mark_x, mark_cells(n) as f64 * cell_w)) }
}

/// Each pane's count, and the titles the marks were last drawn after:
/// a press that comes before the next frame is tested against those.
#[derive(Default)]
pub(crate) struct Marks {
    counts: Vec<u32>,
    titles: Vec<String>,
}

impl Marks {
    /// Take each pane's count, in order; true when one changed.
    pub fn set(&mut self, counts: impl Iterator<Item = u32> + Clone) -> bool {
        if self.counts.iter().copied().eq(counts.clone()) {
            return false;
        }
        self.counts.clear();
        self.counts.extend(counts);
        true
    }

    pub fn count(&self, i: usize) -> u32 {
        self.counts.get(i).copied().unwrap_or(0)
    }

    fn any(&self) -> bool {
        self.counts.iter().any(|n| *n > 0)
    }

    /// The titles about to be drawn.
    pub fn keep_titles<'a>(&mut self, n: usize, title: impl Fn(usize) -> &'a str) {
        if !self.any() {
            return;
        }
        self.titles.resize_with(n, String::new);
        for (i, kept) in self.titles.iter_mut().enumerate() {
            if *kept != title(i) {
                kept.clear();
                kept.push_str(title(i));
            }
        }
    }

    /// The title pane `i`'s mark follows.
    pub fn title(&self, i: usize) -> &str {
        self.titles.get(i).map_or("", String::as_str)
    }

    /// Everything the marks' places depend on that is theirs.
    pub fn hash_into(&self, h: &mut impl Hasher) {
        self.counts.hash(h);
        for i in (0..self.counts.len()).filter(|i| self.counts[*i] > 0) {
            self.title(i).hash(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f64 = 8.0;

    fn at(title: &str, badge: &str, n: u32, w: f64) -> Fit {
        fit((100.0, w), 4.0, title, badge, n, W)
    }

    #[test]
    fn helper_panel_mark_follows_the_name_by_one_cell() {
        let f = at("spg", "", 2, 400.0);
        assert_eq!(f.mark, Some((104.0 + 4.0 * W, 2.0 * W)));
        assert_eq!(at("spg", "", 12, 400.0).mark, Some((104.0 + 4.0 * W, 3.0 * W)));
        assert_eq!(at("spg", "", 0, 400.0).mark, None, "nothing to say");
        // two cells a wide character
        assert_eq!(at("日本", "", 2, 400.0).mark.unwrap().0, 104.0 + 5.0 * W);
    }

    #[test]
    fn helper_panel_mark_takes_its_room_from_the_name() {
        let long = "a-name-much-longer-than-the-strip";
        let without = at(long, "P1", 0, 200.0);
        let with = at(long, "P1", 3, 200.0);
        assert_eq!(with.name_chars + 3, without.name_chars, "a space and `+3`");
        let (x, w) = with.mark.unwrap();
        assert!(x + w <= 300.0 - 4.0 - 2.0 * W, "clear of the badge");
        assert_eq!(at(long, "", 3, 2.0 * W + 8.0).mark, None, "no room even for it");
    }

    #[test]
    fn helper_panel_marks_say_when_a_count_changed() {
        let mut m = Marks::default();
        assert!(m.set([0, 2].into_iter()) && !m.set([0, 2].into_iter()));
        m.keep_titles(2, |i| ["a", "b"][i]);
        assert_eq!((m.count(1), m.count(7), m.title(1)), (2, 0, "b"));
        assert!(m.set([0, 0].into_iter()));
    }
}
