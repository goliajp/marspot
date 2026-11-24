//! Which session a held key's press went to: its release goes there
//! too, whatever is shown by then, and one nobody was told of is never
//! told of its release.

use std::collections::VecDeque;

use marspot::input::LogicalKey;

/// More keys than two hands hold; the oldest is forgotten past it.
const CAP: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Went {
    To(u64),
    Nowhere,
}

pub(super) struct HeldKeys(VecDeque<(LogicalKey, Went)>);

impl Default for HeldKeys {
    fn default() -> Self {
        Self(VecDeque::with_capacity(CAP))
    }
}

impl HeldKeys {
    pub(super) fn went_to(&mut self, key: &LogicalKey, sid: u64) {
        self.note(key, Went::To(sid));
    }

    pub(super) fn swallowed(&mut self, key: &LogicalKey) {
        self.note(key, Went::Nowhere);
    }

    fn note(&mut self, key: &LogicalKey, went: Went) {
        // several keys are this one, held at once each with its own entry
        if *key != LogicalKey::Other {
            self.released(key);
        }
        if self.0.len() == CAP {
            self.0.pop_front();
        }
        self.0.push_back((key.clone(), went));
    }

    pub(super) fn of(&self, key: &LogicalKey) -> Option<Went> {
        self.0.iter().rev().find(|k| k.0 == *key).map(|k| k.1)
    }

    pub(super) fn released(&mut self, key: &LogicalKey) -> Option<Went> {
        let at = self.0.iter().rposition(|k| k.0 == *key)?;
        self.0.remove(at).map(|k| k.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peek_held_keys_are_forgotten_oldest_first_past_the_cap() {
        let mut h = HeldKeys::default();
        let key = |i: usize| LogicalKey::Char(char::from(b'a' + i as u8));
        for i in 0..CAP + 1 {
            h.went_to(&key(i), i as u64);
        }
        assert_eq!(h.released(&key(0)), None, "the oldest");
        assert_eq!(h.released(&key(1)), Some(Went::To(1)));
        assert_eq!(h.released(&key(1)), None, "once");
        assert_eq!(h.of(&key(CAP)), Some(Went::To(CAP as u64)));
        // the same key pressed again replaces its own entry
        h.swallowed(&key(2));
        assert_eq!(h.released(&key(2)), Some(Went::Nowhere));
        assert_eq!(h.of(&key(2)), None);
        // keys that share a name: as many releases as presses, newest first
        h.went_to(&LogicalKey::Other, 1);
        h.went_to(&LogicalKey::Other, 2);
        assert_eq!(h.of(&LogicalKey::Other), Some(Went::To(2)));
        assert_eq!(h.released(&LogicalKey::Other), Some(Went::To(2)));
        assert_eq!(h.released(&LogicalKey::Other), Some(Went::To(1)));
        assert_eq!(h.released(&LogicalKey::Other), None);
    }
}
