//! Which attribute numbers a row holds, as its writes said them.
//!
//! The in-RAM history keeps a row it is handed as the row's own cells
//! when it knows every number in them: then it needs only those
//! numbers' values beside the row, not a renumbering of each cell.  A
//! grid whose history is in RAM has its writes note the numbers here;
//! a grid whose history goes to a file does not, and leaves every row
//! unknown.

/// Slots a row keeps numbers in, one for each value of a number's low
/// four bits: the rows of syntax-highlighted code have six to ten
/// numbers.
pub const ROW_STYLES: usize = 16;

/// The numbers a row's cells hold, each in the slot of its low four
/// bits, 0 in an empty slot.  Two numbers for one slot, and the row is
/// unknown until it is blank again: a note is then one compare and at
/// most one store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RowStyles {
    slots: [u32; ROW_STYLES],
    unknown: bool,
}

impl RowStyles {
    pub const UNKNOWN: RowStyles = RowStyles { slots: [0; ROW_STYLES], unknown: true };

    #[inline(always)]
    pub fn note(&mut self, id: u32) {
        let slot = &mut self.slots[id as usize % ROW_STYLES];
        if *slot == id {
            return;
        }
        // number 0 is the default, in any slot it lands on
        if id == 0 {
            return;
        }
        if *slot == 0 {
            *slot = id;
        } else {
            self.unknown = true;
        }
    }

    /// Every number the row holds, each in the slot of its low four
    /// bits, 0 in the others; `None` when the row's writes said more
    /// than that holds.
    #[inline(always)]
    pub fn known(&self) -> Option<&[u32; ROW_STYLES]> {
        (!self.unknown).then_some(&self.slots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(s: &RowStyles) -> Option<Vec<u32>> {
        s.known().map(|k| k.iter().copied().filter(|&id| id != 0).collect())
    }

    #[test]
    fn a_row_knows_its_numbers_until_two_share_a_slot() {
        let mut s = RowStyles::default();
        assert_eq!(held(&s), Some(vec![]));
        for id in [3, 0, 3, 5, 3, 16] {
            s.note(id);
        }
        assert_eq!(held(&s), Some(vec![16, 3, 5]));
        // one number for each slot still empty
        for id in (32..48).filter(|id| ![0, 3, 5].contains(&(id % 16))) {
            s.note(id);
        }
        assert_eq!(held(&s).map(|v| v.len()), Some(ROW_STYLES));
        s.note(19);
        assert_eq!(held(&s), None, "19 and 3 share a slot");
        s.note(3);
        assert_eq!(held(&s), None, "nothing is known again until the row is blank");
    }
}
