//! Which attribute numbers a row holds, as its writes said them.
//!
//! The in-RAM history keeps a row it is handed as the row's own cells
//! when it knows every number in them: then it needs only those
//! numbers' values beside the row, not a renumbering of each cell.  A
//! grid whose history is in RAM has its writes note the numbers here;
//! a grid whose history goes to a file does not, and leaves every row
//! unknown.

/// How many numbers a row keeps before it gives up knowing them: the
/// rows of syntax-highlighted code have six to ten.
pub const ROW_STYLES: usize = 8;
const UNKNOWN: u8 = u8::MAX;

/// The non-default attribute numbers written into a row since it was
/// last blank; may hold numbers no cell has any more, never misses one
/// a cell has.  `known` gives up once more than [`ROW_STYLES`] went in,
/// or when something wrote the row without saying.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RowStyles {
    ids: [u32; ROW_STYLES],
    len: u8,
}

impl RowStyles {
    /// A row nothing is known about.
    pub const UNKNOWN: RowStyles = RowStyles { ids: [0; ROW_STYLES], len: UNKNOWN };

    /// A cell of the row now holds `id`.
    #[inline(always)]
    pub fn note(&mut self, id: u32) {
        let len = self.len as usize;
        if id == 0 || len == UNKNOWN as usize || self.ids[..len].contains(&id) {
            return;
        }
        if len == ROW_STYLES {
            self.len = UNKNOWN;
        } else {
            self.ids[len] = id;
            self.len += 1;
        }
    }

    /// The numbers, when they are known.
    #[inline(always)]
    pub fn known(&self) -> Option<&[u32]> {
        (self.len != UNKNOWN).then(|| &self.ids[..self.len as usize])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_knows_its_numbers_until_too_many() {
        let mut s = RowStyles::default();
        assert_eq!(s.known(), Some(&[][..]));
        for id in [3, 0, 3, 5, 3] {
            s.note(id);
        }
        assert_eq!(s.known(), Some(&[3, 5][..]));
        for id in 10..10 + ROW_STYLES as u32 - 2 {
            s.note(id);
        }
        assert_eq!(s.known().map(<[u32]>::len), Some(ROW_STYLES));
        s.note(99);
        assert_eq!(s.known(), None);
        s.note(3);
        assert_eq!(s.known(), None, "nothing is known again until the row is blank");
    }
}
