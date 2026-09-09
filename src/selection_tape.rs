//! Selection over a pane whose picture the *program* owns.
//!
//! A program on the alt screen (claudecode) never files a line into
//! marspot's scrollback — the alt screen files nothing, by spec — and
//! it scrolls by repainting the whole screen rather than by asking the
//! terminal to scroll.  A selection anchored the ordinary way (rows up
//! from the live bottom) therefore keeps resolving to the same screen
//! rows while the text under them moves: the box sits still on the
//! glass and a copy takes something other than what was picked.
//!
//! What makes this recoverable is that the repaint IS a clean vertical
//! shift.  Measured on a live claudecode pane by injecting wheel
//! reports and reading the published frames: 3 ticks moved the picture
//! by exactly 2 rows with 54 of 63 rows identical to the frame before,
//! 10 ticks by 16 rows with 39 of the 47 rows that *could* still match
//! doing so — and the same count of ticks the other way put the pane
//! back to the frame it started from, byte for byte.
//!
//! So this tape reads that shift off each frame and keeps the text on
//! a *virtual* line number that survives it.  The selection's ends are
//! stored as virtual lines, and the rows the program has shown are
//! kept so that a copy can still include what has scrolled away.
//!
//! Alive only while a selection stands on such a pane; dropped with
//! the selection.

use std::collections::VecDeque;

/// Rows the tape will hold.  ~1.5 MB of text at 73 columns, and far
/// more than a drag can reach: a selection that has travelled 20,000
/// lines is not one anybody made by hand.  The half further from the
/// selection is dropped first.
const MAX_TAPE_LINES: usize = 20_000;

/// Least rows that must line up before a frame is accepted as a shift
/// of the one before it, as a fraction of the rows that *could* line
/// up (`rows - |shift|`).  A pure scroll comes in far above this; a
/// program redrawing its content does not.
const MIN_ALIGN_NUM: usize = 1;
const MIN_ALIGN_DEN: usize = 2;

/// Least rows that must line up at all, whatever the fraction says.
/// Two matching rows out of three is a coincidence, not a scroll.
const MIN_ALIGN_ROWS: usize = 4;

/// Frames in a row that may fail to align before the tape gives up.
///
/// One unreadable frame is usually not a redraw: a program that does
/// not bracket its repaints in synchronized output (`CSI ?2026h`) can
/// have a half-painted screen published, and half a screen looks
/// exactly like an unrelated one.  The frame after it is whole again
/// and aligns against the last frame that WAS whole, so a miss keeps
/// the reference rather than replacing it.
const MAX_CONSECUTIVE_MISSES: u8 = 3;

pub struct SelectionTape {
    pane_idx: usize,
    /// Virtual line number of screen row 0 in the newest frame.
    base: i64,
    /// Virtual line number of `lines[0]`.
    origin: i64,
    lines: VecDeque<String>,
    /// Row hashes of the newest frame, for the next alignment.
    prev: Vec<u64>,
    /// Selection ends as (column, virtual line).
    pub anchor: (u16, i64),
    pub focus: (u16, i64),
    /// A frame arrived that could not be read as a shift of the one
    /// before it — the program redrew rather than scrolled, and the
    /// tape can no longer say where its lines went.  Folding stops;
    /// what was captured stays captured.
    pub broken: bool,
    /// Frames in a row that did not align.  Reset by any that does.
    misses: u8,
}

fn hash_row(s: &str) -> u64 {
    // FNV-1a: no allocation, no DefaultHasher state to carry around,
    // and this runs over every row of every frame while a selection
    // stands.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// The vertical shift that best explains `cur` as `prev` moved, or
/// `None` when no shift explains enough of the screen.  `cur[r]` is
/// taken to be `prev[r + shift]`, so a positive shift means the
/// content moved UP (newer text arriving at the bottom) and a negative
/// one means it moved DOWN (older text uncovered at the top).
fn best_shift(prev: &[u64], cur: &[u64], blank: u64) -> Option<i64> {
    let n = prev.len() as i64;
    if n == 0 || cur.len() as i64 != n {
        return None;
    }
    let mut best = (0i64, 0usize);
    for s in -(n - 1)..n {
        let mut m = 0usize;
        for r in 0..n {
            let src = r + s;
            if src < 0 || src >= n {
                continue;
            }
            if cur[r as usize] == prev[src as usize] && cur[r as usize] != blank {
                m += 1;
            }
        }
        // Ties go to the smaller shift: a screen with repeated rows
        // (blank runs, box-drawing rules) can align equally well at
        // several distances, and the nearest one is the one a scroll
        // actually made.
        if m > best.1 {
            best = (s, m);
        }
    }
    let (shift, matched) = best;
    let overlap = (n - shift.abs()).max(0) as usize;
    if matched < MIN_ALIGN_ROWS || matched * MIN_ALIGN_DEN < overlap * MIN_ALIGN_NUM {
        return None;
    }
    Some(shift)
}

impl SelectionTape {
    /// Start a tape from the frame the drag began on.  `anchor_row` is
    /// a screen row, `rows` the whole visible picture top to bottom.
    pub fn new(pane_idx: usize, rows: &[String], anchor_col: u16, anchor_row: u16) -> Self {
        let mut t = Self {
            pane_idx,
            base: 0,
            origin: 0,
            lines: VecDeque::new(),
            prev: rows.iter().map(|r| hash_row(r)).collect(),
            anchor: (anchor_col, anchor_row as i64),
            focus: (anchor_col, anchor_row as i64),
            broken: false,
            misses: 0,
        };
        t.write_frame(rows);
        t
    }

    pub fn pane_idx(&self) -> usize {
        self.pane_idx
    }

    /// Fold a new frame in, returning the shift it was read as.  A
    /// frame that is not a shift of the one before breaks the tape and
    /// returns `None`; an unchanged frame returns `Some(0)` and costs
    /// one hash pass.
    pub fn fold(&mut self, rows: &[String]) -> Option<i64> {
        if self.broken {
            return None;
        }
        let cur: Vec<u64> = rows.iter().map(|r| hash_row(r)).collect();
        if cur == self.prev {
            return Some(0);
        }
        let blank = hash_row("");
        let Some(shift) = best_shift(&self.prev, &cur, blank) else {
            // Keep the last frame that DID align as the reference: a
            // half-painted frame must not become the thing the next
            // one is measured against.
            self.misses += 1;
            if self.misses >= MAX_CONSECUTIVE_MISSES {
                self.broken = true;
            }
            return None;
        };
        self.misses = 0;
        self.base += shift;
        self.prev = cur;
        self.write_frame(rows);
        Some(shift)
    }

    /// Point the far end at a screen row of the newest frame.
    pub fn set_focus(&mut self, col: u16, screen_row: u16) {
        self.focus = (col, self.base + screen_row as i64);
    }

    /// Where an end sits in the coordinates the renderer uses: rows up
    /// from the live bottom.  An end that has scrolled off the bottom
    /// saturates at 0 and one off the top runs past `rows - 1`, which
    /// is exactly how the renderer already draws "the selection carries
    /// on past this edge".
    pub fn abs(&self, virt: i64, grid_rows: u16) -> u32 {
        let screen_row = virt - self.base;
        let abs = grid_rows as i64 - 1 - screen_row;
        abs.clamp(0, u32::MAX as i64) as u32
    }

    /// The selected text, read off the tape rather than the screen so
    /// that what has scrolled away is still in it.
    pub fn text(&self, blockwise: bool) -> Option<String> {
        let (a, b) = if (self.anchor.1, self.anchor.0) <= (self.focus.1, self.focus.0) {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        };
        let (lo, hi) = if blockwise {
            (a.0.min(b.0), a.0.max(b.0))
        } else {
            (0, u16::MAX)
        };
        let mut out: Vec<String> = Vec::new();
        for virt in a.1..=b.1 {
            let Some(line) = self.line(virt) else { continue };
            let chars: Vec<char> = line.chars().collect();
            let (from, to) = if blockwise {
                (lo as usize, (hi as usize + 1).min(chars.len()))
            } else {
                let from = if virt == a.1 { a.0 as usize } else { 0 };
                let to = if virt == b.1 {
                    (b.0 as usize + 1).min(chars.len())
                } else {
                    chars.len()
                };
                (from, to)
            };
            let slice: String = if from >= to || from >= chars.len() {
                String::new()
            } else {
                chars[from..to.min(chars.len())].iter().collect()
            };
            out.push(slice.trim_end().to_string());
        }
        while out.last().is_some_and(|l| l.is_empty()) {
            out.pop();
        }
        while out.first().is_some_and(|l| l.is_empty()) {
            out.remove(0);
        }
        if out.is_empty() {
            None
        } else {
            Some(out.join("\n"))
        }
    }

    fn line(&self, virt: i64) -> Option<&String> {
        if virt < self.origin {
            return None;
        }
        self.lines.get((virt - self.origin) as usize)
    }

    fn write_frame(&mut self, rows: &[String]) {
        for (r, text) in rows.iter().enumerate() {
            self.write_line(self.base + r as i64, text);
        }
        self.trim();
    }

    fn write_line(&mut self, virt: i64, text: &str) {
        if self.lines.is_empty() {
            self.origin = virt;
            self.lines.push_back(text.to_string());
            return;
        }
        while virt < self.origin {
            self.lines.push_front(String::new());
            self.origin -= 1;
        }
        while virt >= self.origin + self.lines.len() as i64 {
            self.lines.push_back(String::new());
        }
        let slot = &mut self.lines[(virt - self.origin) as usize];
        // A blank row never erases text the tape already holds.  A
        // program without synchronized output can publish a frame it
        // is halfway through painting, and its not-yet-painted rows
        // arrive as blanks — which read as a zero shift (the painted
        // half still lines up) and would otherwise punch holes in
        // exactly the lines a copy is about to be taken from.  What
        // the user saw is the thing worth keeping.
        if text.is_empty() && !slot.is_empty() {
            return;
        }
        *slot = text.to_string();
    }

    /// Bounded growth: drop from whichever end is further from the
    /// selection, so a long drag never trims what it is selecting.
    fn trim(&mut self) {
        while self.lines.len() > MAX_TAPE_LINES {
            let mid = (self.anchor.1 + self.focus.1) / 2;
            let back = self.origin + self.lines.len() as i64 - 1;
            if (mid - self.origin).abs() > (back - mid).abs() {
                self.lines.pop_back();
            } else {
                self.lines.pop_front();
                self.origin += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(first: usize, rows: usize) -> Vec<String> {
        (first..first + rows).map(|i| format!("line {i}")).collect()
    }

    #[test]
    fn a_line_keeps_its_virtual_number_when_the_program_scrolls() {
        let f0 = frame(0, 10);
        let mut t = SelectionTape::new(0, &f0, 0, 3);
        let anchored = t.anchor.1;
        // The program uncovers older text: everything moves down 2.
        let f1 = frame(0, 10 - 2);
        let mut shifted = vec!["older a".to_string(), "older b".to_string()];
        shifted.extend(f1);
        assert_eq!(t.fold(&shifted), Some(-2));
        assert_eq!(t.anchor.1, anchored, "the anchor is a virtual line");
        assert_eq!(
            t.line(anchored).map(String::as_str),
            Some("line 3"),
            "and it still names the text it was put on"
        );
    }

    #[test]
    fn what_scrolled_away_is_still_in_the_copy() {
        let f0 = frame(0, 6);
        let mut t = SelectionTape::new(0, &f0, 0, 0);
        // Six new lines arrive at the bottom; the first six leave.
        assert_eq!(t.fold(&frame(6, 6)), None, "no overlap left to align on");
        // Fold in overlapping frames instead, one line at a time.
        let mut t = SelectionTape::new(0, &f0, 0, 0);
        for i in 1..=6 {
            assert_eq!(t.fold(&frame(i, 6)), Some(1));
        }
        t.set_focus(6, 5); // last row of the newest frame = "line 11"
        let text = t.text(false).expect("text");
        assert!(text.starts_with("line 0"), "kept the row that scrolled off: {text:?}");
        assert!(text.ends_with("line 11"), "and reaches the newest row: {text:?}");
        assert_eq!(text.lines().count(), 12);
    }

    #[test]
    fn a_repaint_that_is_not_a_scroll_breaks_the_tape_instead_of_guessing() {
        let f0 = frame(0, 10);
        let mut t = SelectionTape::new(0, &f0, 0, 0);
        let unrelated: Vec<String> =
            (0..10).map(|i| format!("something else {i}")).collect();
        for _ in 0..MAX_CONSECUTIVE_MISSES {
            assert_eq!(t.fold(&unrelated), None);
        }
        assert!(t.broken);
        assert_eq!(t.fold(&frame(1, 10)), None, "and stays broken");
    }

    #[test]
    fn one_unreadable_frame_is_ridden_out_against_the_last_good_one() {
        let f0 = frame(0, 10);
        let mut t = SelectionTape::new(0, &f0, 0, 3);
        let anchored = t.anchor.1;
        // A frame published mid-repaint with nothing recognisable in
        // it at all.
        let garbage: Vec<String> = (0..10).map(|i| format!("~{i}~")).collect();
        assert_eq!(t.fold(&garbage), None);
        assert!(!t.broken, "one bad frame is not a redraw");
        // The next whole frame is measured against the last whole one,
        // not against the half — so the shift is still right.
        let mut shifted = vec!["older".to_string()];
        shifted.extend(frame(0, 9));
        assert_eq!(t.fold(&shifted), Some(-1));
        assert_eq!(t.line(anchored).map(String::as_str), Some("line 3"));
    }

    #[test]
    fn a_blank_screen_still_breaks_it_if_it_keeps_coming() {
        let f0 = frame(0, 10);
        let mut t = SelectionTape::new(0, &f0, 0, 0);
        let blanks: Vec<String> = (0..10).map(|_| String::new()).collect();
        for _ in 0..MAX_CONSECUTIVE_MISSES {
            t.fold(&blanks);
        }
        assert!(t.broken);
    }

    #[test]
    fn a_half_painted_frame_does_not_punch_holes_in_what_was_captured() {
        let f0 = frame(0, 10);
        let mut t = SelectionTape::new(0, &f0, 0, 0);
        // The top half is painted, the bottom half has not been yet.
        let half: Vec<String> = (0..10)
            .map(|i| if i < 5 { format!("line {i}") } else { String::new() })
            .collect();
        assert_eq!(t.fold(&half), Some(0), "the painted half still lines up");
        assert_eq!(
            t.line(7).map(String::as_str),
            Some("line 7"),
            "and the rows it had not reached yet keep their text"
        );
    }

    #[test]
    fn an_unchanged_frame_is_a_zero_shift_not_a_break() {
        let f0 = frame(0, 10);
        let mut t = SelectionTape::new(0, &f0, 0, 0);
        assert_eq!(t.fold(&f0), Some(0));
        assert!(!t.broken);
    }

    #[test]
    fn a_blank_screen_cannot_align_and_does_not_pretend_to() {
        let f0 = frame(0, 10);
        let mut t = SelectionTape::new(0, &f0, 0, 0);
        let blanks: Vec<String> = (0..10).map(|_| String::new()).collect();
        assert_eq!(t.fold(&blanks), None, "no shift is readable from it");
    }

    #[test]
    fn an_end_off_the_bottom_reads_as_the_bottom_row() {
        let f0 = frame(10, 10);
        let mut t = SelectionTape::new(0, &f0, 0, 9);
        // The user scrolls back: older text is uncovered at the top and
        // everything already on screen moves down, four rows in all.
        for i in 1..=4 {
            assert_eq!(t.fold(&frame(10 - i, 10)), Some(-1));
        }
        // The anchor sat on the last row, so it is now below the screen
        // and reads as the bottom row — "the selection carries on past
        // this edge", which is what the renderer draws.
        assert_eq!(t.abs(t.anchor.1, 10), 0);
        // A row still on screen keeps its ordinary coordinate.
        assert_eq!(t.abs(t.base + 9, 10), 0);
        assert_eq!(t.abs(t.base, 10), 9);
    }

    #[test]
    fn the_tape_stays_bounded_under_a_very_long_drag() {
        let f0 = frame(0, 10);
        let mut t = SelectionTape::new(0, &f0, 0, 0);
        for i in 1..=(MAX_TAPE_LINES + 500) {
            t.fold(&frame(i, 10));
        }
        assert!(!t.broken);
        assert!(t.lines.len() <= MAX_TAPE_LINES, "len {}", t.lines.len());
    }
}
