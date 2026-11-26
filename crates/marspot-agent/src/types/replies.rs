//! The last replies seen and what was counted for each, so that a
//! reply written as several records is counted once.

use super::record::Tokens;
use super::short_id::ShortId;

/// Replies remembered: one reply's records come one after another, so
/// a short memory is enough.
pub const KEPT: usize = 64;

#[derive(Clone, Copy, Debug)]
pub struct Replies {
    ids: [(ShortId, Tokens); KEPT],
    /// Index of the oldest entry.
    head: usize,
    len: usize,
}

impl Default for Replies {
    fn default() -> Self {
        Replies { ids: [(ShortId::EMPTY, Tokens::default()); KEPT], head: 0, len: 0 }
    }
}

impl Replies {
    /// What a record of reply `id` with counts `t` adds: a reply seen
    /// before counts only its growth.  The oldest reply is forgotten
    /// past [`KEPT`].
    pub fn add(&mut self, id: &str, t: Tokens) -> Tokens {
        let key = ShortId::new(id);
        for k in 0..self.len {
            let (i, was) = &mut self.ids[(self.head + k) % KEPT];
            if *i == key {
                let more = t.beyond(was);
                *was = was.max(&t);
                return more;
            }
        }
        if self.len == KEPT {
            self.head = (self.head + 1) % KEPT;
            self.len -= 1;
        }
        self.ids[(self.head + self.len) % KEPT] = (key, t);
        self.len += 1;
        t
    }

    /// The counts of the newest reply.
    pub fn last(&self) -> Option<Tokens> {
        (self.len > 0).then(|| self.ids[(self.head + self.len - 1) % KEPT].1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_count_growth_and_forget_the_oldest() {
        let t = |o| Tokens { output: o, ..Tokens::default() };
        let mut r = Replies::default();
        assert_eq!(r.add("a", t(5)), t(5));
        assert_eq!(r.add("a", t(8)), t(3));
        assert_eq!(r.add("a", t(2)), t(0), "fewer than counted adds nothing");
        for n in 0..KEPT {
            r.add(&format!("r{n}"), t(1));
        }
        assert_eq!(r.add("a", t(8)), t(8), "forgotten after {KEPT} newer replies");
        assert_eq!(r.last(), Some(t(8)));
    }
}
