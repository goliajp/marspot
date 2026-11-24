//! A grid's attributes, kept once per distinct value instead of once
//! per cell.
//!
//! A screen holds a few dozen distinct attribute sets and thousands of
//! cells, so a cell carries a number into its grid's table instead of
//! the sixteen bytes.  The table holds each value once, finds a value's
//! number through a hash index, and gives numbers no cell uses back.
//!
//! Giving numbers back is a sweep, not a count per number: a count
//! would cost every cell write a read of what it overwrites.  The sweep
//! runs in steps of a few rows, so no single write pays for a whole
//! screen; while it runs, every number written into a cell is marked as
//! it is written, which is what lets rows already swept change under it.
//! A number that was free when nothing used it is only reused after a
//! sweep has ended, and each sweep ending moves [`StyleTable::generation`],
//! so anything that remembers what a number meant can tell when to
//! forget it.


use super::packed_style::FREE;
pub use super::packed_style::PackedStyle;

/// Numbered attribute sets for one grid.
///
/// Two ways to number them.  Shared: one number per distinct value,
/// found through the index, which is what nearly every screen is.
/// Positional: number `i + 1` belongs to cell `i` and holds whatever
/// that cell's attributes are, for a screen with more distinct values
/// than the shared way could hold within the grid's memory bound (see
/// [`limit_for`]).  Number 0 is the default in both.
pub struct StyleTable {
    /// By number.  A free slot has `FREE` set and the next free number
    /// plus one in its high half.
    styles: Vec<PackedStyle>,
    /// Number plus one per slot, 0 for empty; linear probing, a power
    /// of two long, at most half full.  Empty while positional.
    index: Vec<u32>,
    /// The first free number plus one; 0 when none is free.
    free: u32,
    /// Numbers in use, the default's included.
    used: u32,
    /// How many numbers the shared way may hand out.
    limit: u32,
    positional: bool,
    /// Values looked up lately and their numbers, by a few bits of the
    /// value: output changes colour a dozen times a line between the
    /// same few colours, and a hit here is a load and a compare where
    /// the index is a probe and a second load.  An empty entry holds
    /// the default, which is never looked up.
    recent: [(PackedStyle, u32); RECENT],
    /// `used` when the last sweep ended, and new numbers handed out
    /// since: a sweep starts once there are as many new as there were
    /// left, and never for fewer than [`MIN_SWEEP`].
    retained: u32,
    fresh: u32,
    /// While a sweep runs: one bit per number that existed when it
    /// started, set for every number found in a cell or written into
    /// one.  Numbers from `marks_upto` on were handed out during the
    /// sweep and are in use by construction.
    marks: Vec<u64>,
    marks_upto: u32,
    marking: bool,
    /// The grid owes a sweep step; see [`Self::step_due`].
    step_due: bool,
    generation: u64,
}

/// No sweep for fewer new numbers than this: the screens that cycle
/// colours fastest still settle into a few hundred.
pub const MIN_SWEEP: u32 = 256;

/// How many numbers a grid of `cells` cells may give out the shared
/// way.
///
/// The shared way costs eight bytes a number and up to sixteen bytes of
/// index a number (the index is at most half full and a power of two
/// long).  Half the largest power of two not above `cells` keeps the
/// two together under eight bytes a cell, which with the cells' own
/// eight is the sixteen the positional way costs.  Past it the grid
/// numbers positionally.
pub fn limit_for(cells: usize) -> u32 {
    let p = if cells == 0 { 1 } else { 1usize << (usize::BITS - 1 - cells.leading_zeros()) };
    (p / 2).max(1).min(u32::MAX as usize) as u32
}

const RECENT: usize = 16;

#[inline(always)]
fn recent_slot(s: PackedStyle) -> usize {
    (s.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 60) as usize
}

/// Two rounds of mixing, then the top bits: a value's low byte is the
/// same for every colour of a kind and a background alone leaves the
/// low half zero, so one product's low or middle bits pile counted-up
/// colours onto a few slots.  Only a miss in `recent` pays for it.
#[inline(always)]
fn slot_of(s: PackedStyle, mask: usize) -> usize {
    let h = s.0.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let h = (h ^ h >> 32).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h.rotate_left(mask.count_ones()) as usize & mask
}

impl StyleTable {
    pub fn new(cells: usize) -> StyleTable {
        StyleTable {
            styles: vec![PackedStyle::DEFAULT],
            index: Vec::new(),
            free: 0,
            used: 1,
            limit: limit_for(cells),
            positional: false,
            recent: [(PackedStyle::DEFAULT, 0); RECENT],
            retained: 1,
            fresh: 0,
            marks: Vec::new(),
            marks_upto: 0,
            marking: false,
            step_due: false,
            generation: 0,
        }
    }

    /// The value behind a number a cell holds.
    #[inline(always)]
    pub fn get(&self, id: u32) -> PackedStyle {
        debug_assert!(self.styles[id as usize].0 & FREE == 0, "number {id} is free");
        self.styles[id as usize]
    }

    /// The value behind `id`, or the default for a number that is free
    /// now: for a row's list of numbers, which may name one no cell
    /// holds any more.
    #[inline]
    pub fn value_or_default(&self, id: u32) -> PackedStyle {
        match self.styles.get(id as usize) {
            Some(s) if s.0 & FREE == 0 => *s,
            _ => PackedStyle::DEFAULT,
        }
    }

    /// Whether `id` names a value: in range and not free.
    pub fn holds(&self, id: u32) -> bool {
        self.styles.get(id as usize).is_some_and(|s| s.0 & FREE == 0)
    }

    /// While sweeping: whether `id` has been marked, or is too new to
    /// need it.
    pub fn is_marked(&self, id: u32) -> bool {
        id >= self.marks_upto || self.marks[id as usize / 64] & (1 << (id % 64)) != 0
    }

    /// Moves whenever a number may have stopped meaning what it meant.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Take over from `old`: numbers handed out from here on must not
    /// be mistaken for the ones `old` handed out.
    pub(super) fn succeed(&mut self, old: &StyleTable) {
        self.generation = old.generation + 1;
    }

    pub fn is_positional(&self) -> bool {
        self.positional
    }

    /// Every slot by number, free ones included (those hold no value
    /// [`PackedStyle::is_valid`] accepts): what a frame carries.
    pub fn values(&self) -> &[PackedStyle] {
        &self.styles
    }

    /// Slots, used or free.
    pub fn len(&self) -> usize {
        self.styles.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// Numbers in use, the default's included.
    pub fn used(&self) -> u32 {
        self.used
    }

    /// Bytes held: the values, the index, and a running sweep's marks.
    pub fn bytes(&self) -> usize {
        self.styles.capacity() * 8 + self.index.capacity() * 4 + self.marks.capacity() * 8
    }

    /// The number for `s`, handing out a new one if it has none.
    ///
    /// `None` when it has none and the shared way is out of numbers:
    /// the caller sweeps or numbers positionally.  Never called while
    /// positional.
    #[inline(always)]
    pub fn intern(&mut self, s: PackedStyle) -> Option<u32> {
        debug_assert!(!self.positional);
        if s.is_default() {
            return Some(0);
        }
        let slot = recent_slot(s);
        let (held, id) = self.recent[slot];
        if held == s {
            if self.marking {
                self.mark(id);
            }
            return Some(id);
        }
        let id = self.intern_slow(s)?;
        self.recent[slot] = (s, id);
        Some(id)
    }

    #[inline(never)]
    fn intern_slow(&mut self, s: PackedStyle) -> Option<u32> {
        if let Some(id) = self.find(s) {
            if self.marking {
                self.mark(id);
            }
            return Some(id);
        }
        if self.used >= self.limit {
            return None;
        }
        if (self.used as usize + 1) * 2 > self.index.len() {
            self.grow_index();
        }
        let id = if self.free != 0 {
            let id = self.free - 1;
            self.free = (self.styles[id as usize].0 >> 32) as u32;
            self.styles[id as usize] = s;
            id
        } else {
            if self.styles.len() == self.styles.capacity() {
                // grow by doubling, but never past the limit: the bound
                // in `limit_for` is on what is held, not on what is used
                let want = (self.styles.capacity() * 2).max(8).min(self.limit as usize);
                self.styles.reserve_exact(want.saturating_sub(self.styles.len()).max(1));
            }
            self.styles.push(s);
            (self.styles.len() - 1) as u32
        };
        if self.marking && id < self.marks_upto {
            self.mark(id);
        }
        self.used += 1;
        self.fresh += 1;
        // the grid owes a sweep step: one is due to start, or one is
        // running and a new number went out
        self.step_due = self.marking || self.fresh >= self.retained.max(MIN_SWEEP);
        let mask = self.index.len() - 1;
        let mut i = slot_of(s, mask);
        while self.index[i] != 0 {
            i = (i + 1) & mask;
        }
        self.index[i] = id + 1;
        Some(id)
    }

    fn find(&self, s: PackedStyle) -> Option<u32> {
        if self.index.is_empty() {
            return None;
        }
        let mask = self.index.len() - 1;
        let mut i = slot_of(s, mask);
        loop {
            let e = self.index[i];
            if e == 0 {
                return None;
            }
            if self.styles[e as usize - 1] == s {
                return Some(e - 1);
            }
            i = (i + 1) & mask;
        }
    }

    fn grow_index(&mut self) {
        let len = (self.index.len() * 2).max(4);
        self.index.clear();
        self.index.resize(len, 0);
        let mask = len - 1;
        for (id, s) in self.styles.iter().enumerate().skip(1) {
            if s.0 & FREE != 0 {
                continue;
            }
            let mut i = slot_of(*s, mask);
            while self.index[i] != 0 {
                i = (i + 1) & mask;
            }
            self.index[i] = id as u32 + 1;
        }
    }

    /// Whether enough new numbers went out that a sweep should start.
    #[inline(always)]
    pub fn wants_sweep(&self) -> bool {
        !self.marking && !self.positional && self.fresh >= self.retained.max(MIN_SWEEP)
    }

    pub fn is_marking(&self) -> bool {
        self.marking
    }

    /// Whether the grid owes a sweep step since [`Self::stepped`]: a
    /// sweep is due to start, or one is running and a new number went
    /// out.
    #[inline(always)]
    pub fn step_due(&self) -> bool {
        self.step_due
    }

    pub fn stepped(&mut self) {
        self.step_due = false;
    }

    pub fn begin_sweep(&mut self) {
        debug_assert!(!self.marking && !self.positional);
        self.marks_upto = self.styles.len() as u32;
        self.marks.clear();
        self.marks.resize(self.styles.len().div_ceil(64), 0);
        self.marks[0] |= 1;
        self.marking = true;
    }

    /// Note that a cell holds `id`.
    #[inline(always)]
    pub fn mark(&mut self, id: u32) {
        if id < self.marks_upto {
            self.marks[id as usize / 64] |= 1 << (id % 64);
        }
    }

    /// Mark every number in `ids`.
    pub fn mark_all(&mut self, ids: impl Iterator<Item = u32>) {
        for id in ids {
            self.mark(id);
        }
    }

    /// End a sweep that saw every cell: free what nothing marked.
    pub fn end_sweep(&mut self) {
        debug_assert!(self.marking);
        self.marking = false;
        let mut freed = 0;
        for id in 1..self.marks_upto {
            let live = self.marks[id as usize / 64] & (1 << (id % 64)) != 0;
            let s = self.styles[id as usize];
            if live || s.0 & FREE != 0 {
                continue;
            }
            self.unindex(id, s);
            self.styles[id as usize] = PackedStyle(FREE | (self.free as u64) << 32);
            self.free = id + 1;
            freed += 1;
        }
        self.used -= freed;
        self.retained = self.used;
        self.fresh = 0;
        self.recent = [(PackedStyle::DEFAULT, 0); RECENT];
        if freed > 0 {
            self.generation += 1;
        }
    }

    /// Take `id`, holding `s`, out of the index, closing the gap.
    fn unindex(&mut self, id: u32, s: PackedStyle) {
        let mask = self.index.len() - 1;
        let mut i = slot_of(s, mask);
        while self.index[i] != id + 1 {
            i = (i + 1) & mask;
        }
        let mut j = i;
        loop {
            j = (j + 1) & mask;
            let e = self.index[j];
            if e == 0 {
                break;
            }
            let home = slot_of(self.styles[e as usize - 1], mask);
            // `e` may move into the gap at `i` unless its home lies
            // cyclically in (i, j]
            let stays = if i <= j { home > i && home <= j } else { home > i || home <= j };
            if !stays {
                self.index[i] = e;
                i = j;
            }
        }
        self.index[i] = 0;
    }

    /// Back to the default alone, the shared way, keeping the memory
    /// unless it is far more than that needs.
    pub fn reset(&mut self) {
        self.styles.truncate(1);
        self.index.iter_mut().for_each(|e| *e = 0);
        if self.styles.capacity() > 4 * MIN_SWEEP as usize {
            self.styles.shrink_to(MIN_SWEEP as usize);
            self.index = Vec::new();
        }
        self.marks = Vec::new();
        self.free = 0;
        self.used = 1;
        self.retained = 1;
        self.fresh = 0;
        self.recent = [(PackedStyle::DEFAULT, 0); RECENT];
        self.marking = false;
        self.step_due = false;
        self.positional = false;
        self.generation += 1;
    }

    /// A table for a grid of `cells` cells, empty again.
    pub fn resize(&mut self, cells: usize) {
        self.limit = limit_for(cells);
        self.reset();
    }

    /// Number positionally from now on, `values` holding slot `i + 1`
    /// for cell `i` (slot 0 the default).
    pub fn make_positional(&mut self, values: Vec<PackedStyle>) {
        self.styles = values;
        self.index = Vec::new();
        self.marks = Vec::new();
        self.free = 0;
        self.marking = false;
        self.step_due = false;
        self.positional = true;
        self.recent = [(PackedStyle::DEFAULT, 0); RECENT];
        self.generation += 1;
    }

    /// Positional only: cell `idx` holds `s`; the number it should
    /// carry.
    #[inline(always)]
    pub fn set_at(&mut self, idx: usize, s: PackedStyle) -> u32 {
        debug_assert!(self.positional);
        if s.is_default() {
            return 0;
        }
        self.styles[idx + 1] = s;
        idx as u32 + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{CellAttrs, Color};
    use std::collections::HashMap;

    fn next(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    fn attrs_from(seed: u64) -> CellAttrs {
        let color = |k: u64| match k % 3 {
            0 => Color::DEFAULT,
            1 => Color::indexed((k >> 2) as u8),
            _ => Color::rgb((k >> 2) as u8, (k >> 10) as u8, (k >> 18) as u8),
        };
        CellAttrs {
            fg: color(seed),
            bg: color(seed >> 26),
            bold: seed >> 52 & 1 != 0,
            italic: seed >> 53 & 1 != 0,
            underline: seed >> 54 & 1 != 0,
            reverse: seed >> 55 & 1 != 0,
            dim: seed >> 56 & 1 != 0,
            _pad: [0; 3],
        }
    }

    /// Every attribute set packs to one value and back to itself, and
    /// only the default packs to zero.
    #[test]
    fn attributes_survive_packing() {
        let mut s = 0x1234_5678_9ABC_DEF1u64;
        assert_eq!(PackedStyle::pack(&CellAttrs::default()), PackedStyle::DEFAULT);
        let mut seen: HashMap<PackedStyle, CellAttrs> = HashMap::new();
        // an interpreter runs this a thousand times slower
        for _ in 0..if cfg!(miri) { 300 } else { 200_000 } {
            let a = attrs_from(next(&mut s));
            let p = PackedStyle::pack(&a);
            assert_eq!(p.unpack(), a);
            assert!(p.is_valid());
            let cell = crate::grid::Cell { ch: ' ', attrs: a };
            assert_eq!(&p.mem_bytes(false)[..], &crate::grid::Cell::slice_as_bytes(&[cell])[4..]);
            assert_eq!(p.0 & FREE, 0);
            assert_eq!(p.is_default(), a == CellAttrs::default());
            if let Some(prev) = seen.insert(p, a) {
                assert_eq!(prev, a, "two attribute sets share {p:?}");
            }
        }
    }

    /// Values no attribute set packs to are told apart from the rest.
    #[test]
    fn values_pack_cannot_make_are_refused() {
        for bad in [
            3u64,                    // a fourth colour tag
            1 | 5 << 8 | 9 << 16,    // an indexed colour with a second byte
            9 << 8,                  // the default colour with a payload
            FREE,                    // a free slot's marker
            1 << 34,                 // a bit between the background's tag and payload
            3 << 32,                 // a fourth background tag
            1 << 32 | 7 << 48,       // an indexed background with a second byte
        ] {
            assert!(!PackedStyle(bad).is_valid(), "{bad:#x}");
        }
        assert!(PackedStyle(2 | 0xFF_FFFF << 8 | 0x1F << 2).is_valid());
    }

    /// Against a map: numbers stay put while they are in use, a value
    /// is found under the number it was given, and a sweep frees
    /// exactly what nothing marked.
    #[test]
    fn numbers_match_a_map_through_sweeps() {
        let mut s = 0xDEAD_BEEF_0BAD_F00Du64;
        let mut t = StyleTable::new(1 << 20);
        let mut model: HashMap<PackedStyle, u32> = HashMap::new();
        let (rounds, writes) = if cfg!(miri) { (6, 300) } else { (200, 2000) };
        for round in 0..rounds {
            let palette = 1 + next(&mut s) % 600;
            let mut held = Vec::new();
            for _ in 0..writes {
                let a = attrs_from(next(&mut s) % palette * 0x9E37_79B9 + round);
                let p = PackedStyle::pack(&a);
                let id = t.intern(p).expect("room");
                assert_eq!(t.get(id), p);
                if let Some(&old) = model.get(&p) {
                    assert_eq!(old, id, "a value in use changed number");
                }
                model.insert(p, id);
                held.push(id);
            }
            // keep a random half, sweep, and the rest must be gone
            let keep: Vec<u32> = held.iter().copied().filter(|_| next(&mut s).is_multiple_of(2)).collect();
            t.begin_sweep();
            t.mark_all(keep.iter().copied());
            t.end_sweep();
            model.retain(|_, id| *id == 0 || keep.contains(id));
            assert_eq!(t.used() as usize, 1 + model.values().filter(|&&id| id != 0).count());
            for (p, &id) in &model {
                assert_eq!(t.get(id), *p);
                assert_eq!(t.find(*p).unwrap_or(0), id, "the index lost {p:?}");
            }
            let entries = t.index.iter().filter(|&&e| e != 0).count();
            assert_eq!(entries, t.used() as usize - 1, "the index holds freed numbers");
        }
    }

    /// A number handed out while a sweep runs survives it unmarked; one
    /// looked up again during it is marked by the lookup.
    #[test]
    fn a_sweep_keeps_what_was_written_while_it_ran() {
        let mut t = StyleTable::new(1 << 16);
        let a = t.intern(PackedStyle(1 << 8)).unwrap();
        let b = t.intern(PackedStyle(2 << 8)).unwrap();
        let gone = t.intern(PackedStyle(3 << 8)).unwrap();
        t.begin_sweep();
        assert_eq!(t.intern(PackedStyle(1 << 8)), Some(a));
        let c = t.intern(PackedStyle(4 << 8)).unwrap();
        t.mark(b);
        t.end_sweep();
        for (id, v) in [(a, 1), (b, 2), (c, 4)] {
            assert_eq!(t.get(id), PackedStyle(v << 8));
        }
        assert_eq!(t.find(PackedStyle(3 << 8)), None);
        assert_eq!(t.intern(PackedStyle(5 << 8)), Some(gone), "the freed number is reused");
    }

    /// Values that differ only in a colour's payload spread over the
    /// whole index: each entry sits a probe or two from its home slot.
    /// A hash whose low bits ignore those bytes piles every value onto
    /// a few homes, and each new number then probes a long run.
    #[test]
    fn colours_counted_up_spread_over_the_index() {
        let n: u64 = if cfg!(miri) { 2000 } else { 1 << 16 };
        let shapes: [fn(u64) -> CellAttrs; 3] = [
            |k| CellAttrs { bg: Color::rgb((k >> 16) as u8, (k >> 8) as u8, k as u8), ..CellAttrs::default() },
            |k| CellAttrs { fg: Color::rgb((k >> 16) as u8, (k >> 8) as u8, k as u8), ..CellAttrs::default() },
            |k| CellAttrs { fg: Color::indexed(k as u8), bg: Color::indexed((k >> 8) as u8), ..CellAttrs::default() },
        ];
        for (shape, attrs) in shapes.iter().enumerate() {
            let mut t = StyleTable::new(1 << 20);
            for k in 1..=n {
                t.intern(PackedStyle::pack(&attrs(k))).expect("room");
            }
            let mask = t.index.len() - 1;
            let (mut entries, mut probes) = (0u64, 0u64);
            for (j, &e) in t.index.iter().enumerate() {
                if e != 0 {
                    entries += 1;
                    probes += ((j.wrapping_sub(slot_of(t.styles[e as usize - 1], mask))) & mask) as u64 + 1;
                }
            }
            let mean = probes as f64 / entries as f64;
            assert!(mean < 2.0, "shape {shape}: {mean:.1} probes a lookup over {entries} entries");
        }
    }

    /// The shared way stops at its limit, and the limit keeps values
    /// and index within eight bytes a cell.
    #[test]
    fn the_shared_way_stays_within_eight_bytes_a_cell() {
        let sizes: &[usize] = if cfg!(miri) { &[4, 100] } else { &[4, 100, 4758, 1 << 15, 262_144] };
        for &cells in sizes {
            let mut t = StyleTable::new(cells);
            let mut n = 0u64;
            while t.intern(PackedStyle((n + 1) << 8)).is_some() {
                n += 1;
            }
            assert_eq!(t.used(), limit_for(cells));
            let bytes = t.styles.len() * 8 + t.index.len() * 4;
            assert!(bytes <= 8 * cells.max(2), "{cells} cells: {bytes} bytes");
        }
    }
}
