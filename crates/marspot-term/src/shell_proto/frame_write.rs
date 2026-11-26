//! Writing frames from a borrowed payload.

use std::io::{self, IoSlice, Write};

use super::{HEADER_LEN, MAGIC, MsgType};

/// The header of a frame carrying `len` bytes of payload.
pub fn frame_header(msg_type: MsgType, len: usize) -> [u8; HEADER_LEN] {
    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    header[4..8].copy_from_slice(&(msg_type as u32).to_le_bytes());
    header[8..12].copy_from_slice(&(len as u32).to_le_bytes());
    header
}

/// Write one frame from a borrowed payload: the bytes
/// `Frame::new(msg_type, payload.to_vec()).write_to(w)` writes.  Header
/// and payload go out in one vectored write, so a socket sees one
/// syscall per frame unless the kernel takes only part of it.
pub fn write_frame<W: Write>(w: &mut W, msg_type: MsgType, payload: &[u8]) -> io::Result<usize> {
    let header = frame_header(msg_type, payload.len());
    let mut slices = [IoSlice::new(&header), IoSlice::new(payload)];
    let mut rest = &mut slices[..];
    while rest.iter().any(|s| !s.is_empty()) {
        match w.write_vectored(rest) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => IoSlice::advance_slices(&mut rest, n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(HEADER_LEN + payload.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Takes at most `max` bytes per call, from the first non-empty
    /// slice only, as `Write`'s default `write_vectored` does.
    struct Stingy {
        out: Vec<u8>,
        max: usize,
        calls: usize,
    }

    impl Write for Stingy {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            let n = buf.len().min(self.max);
            self.out.extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn expected(msg_type: MsgType, payload: &[u8]) -> Vec<u8> {
        let mut v = frame_header(msg_type, payload.len()).to_vec();
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn partial_writes_still_put_the_whole_frame_out_in_order() {
        let payload: Vec<u8> = (0..=40).collect();
        for max in [1, 5, 11, 12, 13, 100] {
            for p in [&payload[..], &[][..], &payload[..1]] {
                let mut w = Stingy { out: Vec::new(), max, calls: 0 };
                assert_eq!(write_frame(&mut w, MsgType::KeyEvent, p).unwrap(), HEADER_LEN + p.len());
                assert_eq!(w.out, expected(MsgType::KeyEvent, p), "max {max}, {} B", p.len());
            }
        }
    }

    /// Takes every slice it is handed, as a socket with room does.
    struct Vectored {
        out: Vec<u8>,
        calls: usize,
    }

    impl Write for Vectored {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            unreachable!("frames are written vectored")
        }
        fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
            self.calls += 1;
            bufs.iter().for_each(|b| self.out.extend_from_slice(b));
            Ok(bufs.iter().map(|b| b.len()).sum())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_writer_with_room_gets_a_frame_in_one_call() {
        let payload = [7u8; 30];
        let mut w = Vectored { out: Vec::new(), calls: 0 };
        write_frame(&mut w, MsgType::KeyEvent, &payload).unwrap();
        assert_eq!(w.calls, 1);
        assert_eq!(w.out, expected(MsgType::KeyEvent, &payload));
    }

    #[test]
    fn a_writer_that_takes_nothing_is_an_error() {
        let mut w = Stingy { out: Vec::new(), max: 0, calls: 0 };
        let e = write_frame(&mut w, MsgType::KeyEvent, b"x").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::WriteZero);
        assert_eq!(w.calls, 1);
    }
}
