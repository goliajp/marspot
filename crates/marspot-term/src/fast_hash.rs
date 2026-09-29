//! Tiny, dependency-free `Hasher` for small fixed-size keys.  Used by
//! the per-frame hot HashMaps in the renderer (atlas / font cache /
//! per-cell glyph lookup) where the default SipHash dominates total
//! `get`/`insert` cost.
//!
//! Algorithm: the FxHash variant the rustc compiler uses internally
//! for the same reason.  Each 8-byte chunk is xor'd into the state
//! after a 5-bit rotate, then multiplied by a fixed constant; the
//! tail bytes get the same treatment.  No DoS resistance — these
//! maps are NEVER fed adversarial keys (the keys are
//! `(font_id, glyph)` pairs derived from the active grid contents).
//!
//! Why hand-roll instead of pulling `rustc-hash`: the algorithm is
//! ~30 lines, the marspot self-build principle says don't add a
//! library when the work is "obviously a few lines of code"; the
//! comment block above is longer than the implementation.

use std::hash::{BuildHasherDefault, Hasher};

const FX_K: u64 = 0xf1_357a_ea2e_62a9_c5_u64;

/// Hasher for HashMaps whose keys are SMALL and TRUSTED.
#[derive(Default, Clone)]
pub struct FxHasher {
    state: u64,
}

impl FxHasher {
    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.state = (self.state.rotate_left(5) ^ n).wrapping_mul(FX_K);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.state
    }

    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        while bytes.len() >= 8 {
            let chunk = u64::from_ne_bytes(bytes[..8].try_into().unwrap());
            self.write_u64(chunk);
            bytes = &bytes[8..];
        }
        if bytes.len() >= 4 {
            let chunk = u32::from_ne_bytes(bytes[..4].try_into().unwrap()) as u64;
            self.write_u64(chunk);
            bytes = &bytes[4..];
        }
        if bytes.len() >= 2 {
            let chunk = u16::from_ne_bytes(bytes[..2].try_into().unwrap()) as u64;
            self.write_u64(chunk);
            bytes = &bytes[2..];
        }
        if let [b] = *bytes {
            self.write_u64(b as u64);
        }
    }

    // Specialise the common short-key shapes the renderer hammers so
    // the dispatch never has to copy through `write(&[u8])`.
    #[inline]
    fn write_u8(&mut self, n: u8) {
        self.write_u64(n as u64);
    }
    #[inline]
    fn write_u16(&mut self, n: u16) {
        self.write_u64(n as u64);
    }
    #[inline]
    fn write_u32(&mut self, n: u32) {
        self.write_u64(n as u64);
    }
    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
}

/// `BuildHasher` shorthand.  Use as `HashMap<K, V, FxBuildHasher>`.
pub type FxBuildHasher = BuildHasherDefault<FxHasher>;

/// Same shape as `std::collections::HashMap` but with [`FxBuildHasher`]
/// pre-wired — the renderer's hot maps want this everywhere.
pub type FxHashMap<K, V> = std::collections::HashMap<K, V, FxBuildHasher>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::Hash;

    fn hash<H: Hash>(h: H) -> u64 {
        let mut s = FxHasher::default();
        h.hash(&mut s);
        s.finish()
    }

    #[test]
    fn distinct_short_keys_distinct_hashes() {
        let a = hash((1u32, 2u16));
        let b = hash((1u32, 3u16));
        let c = hash((2u32, 2u16));
        assert!(a != b);
        assert!(a != c);
        assert!(b != c);
    }

    #[test]
    fn hashmap_works() {
        let mut m: FxHashMap<u64, &'static str> = FxHashMap::default();
        m.insert(1, "a");
        m.insert(2, "b");
        assert_eq!(m.get(&1), Some(&"a"));
        assert_eq!(m.get(&2), Some(&"b"));
        assert_eq!(m.get(&3), None);
    }
}
