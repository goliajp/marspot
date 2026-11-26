//! An id held inline: a task, a tool call or a reply.

const INLINE: usize = 32;
/// Bytes of a longer id kept as they are; the rest of the room holds a
/// hash of the whole id.
const HEAD: usize = INLINE - 8;

/// An id of up to 32 bytes, as it is; a longer one is its first 24
/// bytes and a 64-bit hash of all of it, with its length.  Two ids are
/// equal exactly when their strings are, unless two long ids share
/// their length, their first 24 bytes and their hash.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShortId {
    len: u32,
    bytes: [u8; INLINE],
}

impl ShortId {
    pub const EMPTY: ShortId = ShortId { len: 0, bytes: [0; INLINE] };

    pub fn new(s: &str) -> ShortId {
        let b = s.as_bytes();
        let mut id = ShortId { len: u32::try_from(b.len()).unwrap_or(u32::MAX), bytes: [0; INLINE] };
        if b.len() <= INLINE {
            id.bytes[..b.len()].copy_from_slice(b);
        } else {
            id.bytes[..HEAD].copy_from_slice(&b[..HEAD]);
            id.bytes[HEAD..].copy_from_slice(&fnv1a(b).to_le_bytes());
        }
        id
    }

    pub fn is(&self, s: &str) -> bool {
        *self == ShortId::new(s)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The id as it was given, when it fitted whole.
    pub fn as_str(&self) -> Option<&str> {
        let n = usize::try_from(self.len).ok().filter(|n| *n <= INLINE)?;
        std::str::from_utf8(&self.bytes[..n]).ok()
    }
}

fn fnv1a(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325, |h, x| (h ^ u64::from(*x)).wrapping_mul(0x0100_0000_01b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_keeps_short_ids_and_tells_long_ones_apart() {
        let a = ShortId::new("toolu_01S2MBywhqmQwLfGXisNqqZL");
        assert_eq!(a.as_str(), Some("toolu_01S2MBywhqmQwLfGXisNqqZL"));
        assert!(a.is("toolu_01S2MBywhqmQwLfGXisNqqZL"));
        let long = "x".repeat(40);
        let other = format!("{}y", "x".repeat(39));
        assert_ne!(ShortId::new(&long), ShortId::new(&other), "same head, same length, other tail");
        assert_eq!(ShortId::new(&long), ShortId::new(&long));
        assert_eq!(ShortId::new(&long).as_str(), None);
        assert!(ShortId::new("").is_empty());
    }
}
