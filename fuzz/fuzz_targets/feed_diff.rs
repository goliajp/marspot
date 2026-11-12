#![no_main]
//! Fuzz the terminal's bulk feed against its reference feed.
//!
//! Contract under test: whatever the bytes and wherever the reads end,
//! `Terminal::feed` leaves the same screen, history, cursor, modes and
//! replies as feeding every byte through the state machine and every
//! character through the per-character path — and neither panics.
//!
//! The first two bytes pick the grid size and the read size, so the
//! fuzzer can move a boundary through a sequence it has found.

use libfuzzer_sys::fuzz_target;
use marspot::terminal::Terminal;

const SIZES: [(u16, u16); 8] =
    [(80, 24), (122, 39), (20, 5), (8, 4), (7, 3), (3, 2), (2, 2), (40, 2)];

fuzz_target!(|data: &[u8]| {
    let Some((&[shape, cut], body)) = data.split_first_chunk::<2>() else {
        return;
    };
    let (cols, rows) = SIZES[(shape & 7) as usize];
    let chunk = if cut == 255 { usize::MAX } else { cut as usize % 67 + 1 };
    let mut fast = Terminal::new(cols, rows);
    let mut slow = Terminal::new(cols, rows);
    if shape & 0x80 != 0 {
        fast.set_render_markup(true);
        slow.set_render_markup(true);
    }
    for part in body.chunks(chunk.min(body.len().max(1))) {
        fast.feed(part);
        slow.feed_reference(part);
    }
    if let Some(d) = fast.first_difference(&slow) {
        panic!("the bulk feed and the reference feed disagree: {d}");
    }
});
