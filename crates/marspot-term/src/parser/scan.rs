//! Byte scans the bulk feed path runs ahead of the state machine.

/// Length of the leading run of printable ASCII (0x20..=0x7E).
#[inline]
pub fn printable_run_len(bytes: &[u8]) -> usize {
    let mut i = 0;
    #[cfg(target_arch = "aarch64")]
    {
        use core::arch::aarch64::*;
        while i + 16 <= bytes.len() {
            // SAFETY: NEON is part of the aarch64 baseline, and the
            // loop condition keeps the 16 bytes read inside `bytes`.
            let mask = unsafe {
                let v = vld1q_u8(bytes.as_ptr().add(i));
                // printable iff (b - 0x20) < 0x5F as unsigned bytes
                let ok = vcltq_u8(vsubq_u8(v, vdupq_n_u8(0x20)), vdupq_n_u8(0x5F));
                // four mask bits per byte, in byte order
                vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(
                    vreinterpretq_u16_u8(ok),
                )))
            };
            if mask != u64::MAX {
                return i + ((!mask).trailing_zeros() / 4) as usize;
            }
            i += 16;
        }
    }
    while i < bytes.len() && bytes[i].wrapping_sub(0x20) < 0x5F {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::printable_run_len;

    fn reference(bytes: &[u8]) -> usize {
        bytes
            .iter()
            .position(|b| !(0x20..=0x7E).contains(b))
            .unwrap_or(bytes.len())
    }

    #[test]
    fn the_run_ends_at_the_first_byte_that_is_not_printable() {
        // every stop byte at every offset of a buffer long enough to
        // cross two vector blocks and leave a scalar tail
        for len in 0..40usize {
            let clean = vec![b'a'; len];
            assert_eq!(printable_run_len(&clean), len);
            for at in 0..len {
                for stop in [0x00u8, 0x0A, 0x1B, 0x1F, 0x7F, 0x80, 0xC3, 0xE4, 0xF0, 0xFF] {
                    let mut b = clean.clone();
                    b[at] = stop;
                    assert_eq!(printable_run_len(&b), at, "len {len} stop {stop:#x} at {at}");
                }
            }
        }
    }

    #[test]
    fn every_byte_value_is_classified_like_the_range_test() {
        for b in 0..=255u8 {
            let mut buf = [b' '; 33];
            for at in [0usize, 7, 15, 16, 31, 32] {
                buf[at] = b;
                assert_eq!(printable_run_len(&buf), reference(&buf), "byte {b:#x} at {at}");
                buf[at] = b' ';
            }
        }
    }
}
