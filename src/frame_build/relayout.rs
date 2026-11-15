//! Rearranging a list of instances around the pieces that changed.

/// A run of instances in one list, first and one past the last.
pub(crate) type Span = (u32, u32);

/// A caller that never empties its list of changed runs must not make
/// it grow: past this many it becomes one run covering the list.
const MAX_SPANS: usize = 32;

/// Rearrange `list` so each piece sits where the new layout puts it:
/// a kept piece is moved if what is before it changed length, a
/// replaced one is copied in from `fresh`.  Pushes onto `dirty` every
/// run of the list that is not what it was; a replaced piece that came
/// out the same, in the same place, is not one.
///
/// `old_ends[i]` and `new_ends[i]` are where piece `i` ends in the old
/// and the new layout; `fresh_ends[i]` is `Some(end in fresh)` for a
/// replaced piece, whose instances are the ones before that end and
/// after the previous replaced piece's.
pub(crate) fn relayout<T: Copy + PartialEq>(
    list: &mut Vec<T>,
    old_ends: &[u32],
    new_ends: &[u32],
    fresh: &[T],
    fresh_ends: &[Option<u32>],
    dirty: &mut Vec<Span>,
) {
    let n = old_ends.len();
    let start = |ends: &[u32], i: usize| if i == 0 { 0 } else { ends[i - 1] as usize };
    let new_len = new_ends.last().map_or(0, |&e| e as usize);
    list.reserve(new_len.saturating_sub(list.len()));
    let base = list.as_mut_ptr();
    let mut mark = |lo: usize, hi: usize| {
        if lo == hi {
            return;
        }
        match dirty.last_mut() {
            Some(last) if last.1 as usize == lo => last.1 = hi as u32,
            _ => dirty.push((lo as u32, hi as u32)),
        }
    };
    // SAFETY: every range below lies inside the list's capacity: old
    // ranges inside its length, new ones inside `new_len`, which was
    // just reserved.  A kept piece is moved with `copy`, which allows
    // the two ranges to overlap.  Pieces moving right go last to
    // first and pieces moving left first to last, so none is written
    // over before it is read: the new layout keeps the pieces in
    // order, so a piece's new place never reaches a later piece that
    // has yet to move left, nor an earlier one yet to move right.
    // For the same reason a replaced piece that has not moved still
    // holds its old instances when it is compared.  After the loops
    // every instance below `new_len` has been written, by a move, a
    // copy from `fresh`, or by being left where it was.
    unsafe {
        for i in (0..n).rev() {
            let (os, ns) = (start(old_ends, i), start(new_ends, i));
            if fresh_ends[i].is_none() && ns > os {
                std::ptr::copy(base.add(os), base.add(ns), old_ends[i] as usize - os);
            }
        }
        let mut fresh_at = 0usize;
        for i in 0..n {
            let (os, ns) = (start(old_ends, i), start(new_ends, i));
            let old_len = old_ends[i] as usize - os;
            match fresh_ends[i] {
                None => {
                    if ns < os {
                        std::ptr::copy(base.add(os), base.add(ns), old_len);
                    }
                    if ns != os {
                        mark(ns, ns + old_len);
                    }
                }
                Some(end) => {
                    let piece = &fresh[fresh_at..end as usize];
                    fresh_at = end as usize;
                    let same = ns == os
                        && piece.len() == old_len
                        && std::slice::from_raw_parts(base.add(os) as *const T, old_len) == piece;
                    if !same {
                        std::ptr::copy_nonoverlapping(piece.as_ptr(), base.add(ns), piece.len());
                        mark(ns, ns + piece.len());
                    }
                }
            }
        }
        list.set_len(new_len);
    }
    if dirty.len() > MAX_SPANS {
        dirty.clear();
        dirty.push((0, new_len as u32));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lay `pieces` out from nothing, the way a full rebuild would.
    fn flat(pieces: &[Vec<u32>]) -> Vec<u32> {
        pieces.iter().flatten().copied().collect()
    }

    fn ends(pieces: &[Vec<u32>]) -> Vec<u32> {
        let mut at = 0;
        pieces.iter().map(|p| { at += p.len() as u32; at }).collect()
    }

    /// Replace the pieces named in `changes` and check the list is
    /// what a rebuild from nothing gives, and that everything that
    /// moved or changed is reported.
    fn check(old: &[Vec<u32>], changes: &[(usize, Vec<u32>)]) {
        let mut new: Vec<Vec<u32>> = old.to_vec();
        let mut fresh = Vec::new();
        let mut fresh_ends = vec![None; old.len()];
        for (i, piece) in changes {
            new[*i] = piece.clone();
        }
        for (i, piece) in new.iter().enumerate() {
            if changes.iter().any(|(c, _)| c == &i) {
                fresh.extend_from_slice(piece);
                fresh_ends[i] = Some(fresh.len() as u32);
            }
        }
        let before = flat(old);
        let mut list = before.clone();
        let mut dirty = Vec::new();
        relayout(&mut list, &ends(old), &ends(&new), &fresh, &fresh_ends, &mut dirty);
        assert_eq!(list, flat(&new), "old {old:?} changes {changes:?}");
        // outside the reported runs the list is what it was
        for (i, v) in list.iter().enumerate() {
            let reported = dirty.iter().any(|&(lo, hi)| (lo as usize..hi as usize).contains(&i));
            assert!(reported || before.get(i) == Some(v), "index {i} changed unreported: {dirty:?}");
        }
    }

    #[test]
    fn a_patched_list_is_the_list_built_from_nothing() {
        let old = vec![vec![1, 2], vec![3, 4, 5], vec![], vec![6], vec![7, 8, 9, 10], vec![11]];
        // same length, in place
        check(&old, &[(1, vec![30, 40, 50])]);
        // one piece grows: everything after it moves right
        check(&old, &[(1, vec![30, 40, 50, 60, 70])]);
        // one piece shrinks to nothing: everything after it moves left
        check(&old, &[(4, vec![])]);
        // an empty piece gains instances
        check(&old, &[(2, vec![20, 21])]);
        // a grow then a shrink: pieces between move right, after move left
        check(&old, &[(0, vec![1, 2, 100, 101, 102]), (4, vec![7])]);
        // a shrink then a grow
        check(&old, &[(1, vec![]), (3, vec![60, 61, 62, 63, 64, 65]), (5, vec![])]);
        // every piece replaced
        check(&old, &(0..6).map(|i| (i, vec![i as u32; i + 1])).collect::<Vec<_>>());
        // nothing replaced
        check(&old, &[]);
    }

    #[test]
    fn only_what_changed_or_moved_is_reported() {
        let old = vec![vec![1, 2], vec![3, 4, 5], vec![6], vec![7, 8]];
        let mut list = flat(&old);
        let mut dirty = Vec::new();
        // piece 1 replaced by one of the same length: nothing else moves
        relayout(&mut list, &ends(&old), &ends(&old), &[30, 40, 50], &[None, Some(3), None, None], &mut dirty);
        assert_eq!(dirty, vec![(2, 5)]);
    }

    #[test]
    fn a_piece_replaced_by_the_same_instances_is_not_reported() {
        let old = vec![vec![1, 2], vec![3, 4, 5], vec![6]];
        let mut list = flat(&old);
        let mut dirty = Vec::new();
        relayout(&mut list, &ends(&old), &ends(&old), &[1, 2, 6], &[Some(2), None, Some(3)], &mut dirty);
        assert_eq!(list, flat(&old));
        assert!(dirty.is_empty(), "{dirty:?}");
    }

    #[test]
    fn the_changed_runs_stay_bounded() {
        let old: Vec<Vec<u32>> = (0..200).map(|i| vec![i]).collect();
        let mut list = flat(&old);
        let mut dirty = Vec::new();
        // every other piece replaced: a hundred separate runs
        let fresh: Vec<u32> = (0..100).map(|i| 1000 + i).collect();
        let fresh_ends: Vec<Option<u32>> = (0..200).map(|i| (i % 2 == 0).then_some(i / 2 + 1)).collect();
        relayout(&mut list, &ends(&old), &ends(&old), &fresh, &fresh_ends, &mut dirty);
        assert_eq!(dirty, vec![(0, 200)]);
    }
}
