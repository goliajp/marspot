//! Random bytes from the kernel, for message ids, socket names and
//! WebSocket keys.  None of these guard a secret; they only need to
//! not collide.

pub fn fill(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(256) {
        // getentropy only fails for lengths over 256 or a bad pointer.
        let rc = unsafe { libc::getentropy(chunk.as_mut_ptr().cast(), chunk.len()) };
        assert_eq!(rc, 0, "getentropy failed");
    }
}

pub fn hex(n: usize) -> String {
    let mut b = vec![0u8; n.div_ceil(2)];
    fill(&mut b);
    let mut s: String = b.iter().map(|x| format!("{x:02x}")).collect();
    s.truncate(n);
    s
}

/// A version-4 UUID, the form Claude Code uses for `msg_id`.
pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_has_the_v4_shape() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(u, uuid_v4());
    }

    #[test]
    fn hex_has_the_asked_length() {
        assert_eq!(hex(8).len(), 8);
        assert_eq!(hex(7).len(), 7);
    }
}
