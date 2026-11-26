//! The text a key event carries, held without the heap when it is short.
//!
//! A keystroke types one character, rarely two (a dead key AppKit could
//! not compose with the next one), and an IME commit of a word or two is
//! a handful of CJK characters.  All of that fits inline, so the key path
//! from AppKit to the PTY can move it around by value.  A longer commit
//! (a pasted-in sentence through the IME, a long emoji sequence) is still
//! carried whole, on the heap: correct, and too rare to be a path.

use std::fmt;
use std::ops::Deref;

/// Bytes held inline.  22 keeps the type the size of a `String` (24),
/// so an event carrying it is no bigger than before, and holds seven
/// CJK characters or the longest common ZWJ family emoji (18 bytes).
pub const INLINE: usize = 22;

#[derive(Clone)]
pub struct KeyText(Repr);

#[derive(Clone)]
enum Repr {
    Inline { len: u8, buf: [u8; INLINE] },
    Heap(Box<str>),
}

impl KeyText {
    /// The empty text.
    pub const EMPTY: KeyText = KeyText(Repr::Inline {
        len: 0,
        buf: [0; INLINE],
    });

    /// `s`, inline when it fits.
    pub fn new(s: &str) -> Self {
        let b = s.as_bytes();
        if b.len() <= INLINE {
            let mut buf = [0u8; INLINE];
            buf[..b.len()].copy_from_slice(b);
            KeyText(Repr::Inline {
                len: b.len() as u8,
                buf,
            })
        } else {
            KeyText(Repr::Heap(s.into()))
        }
    }

    pub fn as_str(&self) -> &str {
        match &self.0 {
            // only ever filled from a `&str`, cut at its own length
            Repr::Inline { len, buf } => unsafe {
                std::str::from_utf8_unchecked(&buf[..*len as usize])
            },
            Repr::Heap(s) => s,
        }
    }

    /// Whether this text is held off the heap.
    pub fn is_inline(&self) -> bool {
        matches!(self.0, Repr::Inline { .. })
    }
}

impl Default for KeyText {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl Deref for KeyText {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for KeyText {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for KeyText {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for KeyText {
    fn from(s: String) -> Self {
        if s.len() <= INLINE {
            Self::new(&s)
        } else {
            KeyText(Repr::Heap(s.into_boxed_str()))
        }
    }
}

impl From<char> for KeyText {
    fn from(c: char) -> Self {
        Self::new(c.encode_utf8(&mut [0u8; 4]))
    }
}

impl PartialEq for KeyText {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for KeyText {}

impl PartialEq<str> for KeyText {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for KeyText {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl fmt::Debug for KeyText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for KeyText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_stays_inline_and_long_text_is_whole() {
        assert_eq!(
            std::mem::size_of::<KeyText>(),
            std::mem::size_of::<String>()
        );
        for s in ["", "a", "é", "日本語", "👩\u{200d}👩\u{200d}👧", "\u{1b}"] {
            let t = KeyText::new(s);
            assert!(t.is_inline(), "{s:?} went to the heap");
            assert_eq!(t.as_str(), s);
        }
        let cap = "x".repeat(INLINE);
        assert!(KeyText::new(&cap).is_inline());
        assert!(KeyText::from(cap.clone()).is_inline());
        // one byte past the inline room, ascii and with a CJK tail
        for over in ["x".repeat(INLINE + 1), format!("ab{}", "日".repeat(7))] {
            assert_eq!(over.len(), 23);
            for t in [KeyText::new(&over), KeyText::from(over.clone())] {
                assert!(!t.is_inline(), "{over:?} stayed inline");
                assert_eq!(t.as_str(), over);
            }
        }
        let long = "这是一段通过输入法一次提交的长句子";
        let t = KeyText::from(long.to_string());
        assert!(!t.is_inline());
        assert_eq!(&*t, long);
        assert_eq!(KeyText::from('ß'), "ß");
    }
}
