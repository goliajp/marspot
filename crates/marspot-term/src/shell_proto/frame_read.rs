//! Reading frames off a stream.

use std::io::{self, Read};

use super::{Frame, HEADER_LEN, MAGIC, MAX_PAYLOAD_LEN, MsgType};

/// One frame off the wire, as [`Frame::read_next`] found it.
#[derive(Debug)]
pub enum Next {
    Frame(Frame),
    /// A frame of a type this build does not know, `u32` the type: its
    /// payload has been read and dropped.
    Unknown(u32),
}

impl Frame {
    /// Read one frame off `r`.  Forward-compatible: a frame whose
    /// `msg_type` this build doesn't recognise (e.g. a new variant
    /// added in a future release) is **silently skipped** — its
    /// payload is consumed and the loop reads the next frame.  Magic /
    /// length-cap / IO errors are still hard errors.
    ///
    /// This makes peer additions of new message types safe across
    /// version skew: the old reader stays alive instead of erroring
    /// and tearing down the control channel.
    ///
    /// A reader that checks for something between two frames (a stop
    /// request) uses [`Self::read_next`] instead: skipping here blocks
    /// for the frame after the unknown one, past that check.
    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Option<Self>> {
        loop {
            match Self::read_next(r)? {
                None => return Ok(None),
                Some(Next::Frame(f)) => return Ok(Some(f)),
                Some(Next::Unknown(_)) => continue,
            }
        }
    }

    /// Read exactly one frame off `r`, known or not; `None` at a clean
    /// end between two frames.
    pub fn read_next<R: Read>(r: &mut R) -> io::Result<Option<Next>> {
        let mut header = [0u8; HEADER_LEN];
        match read_exact_or_eof(r, &mut header)? {
            ReadEnd::Eof => return Ok(None),
            ReadEnd::Full => {}
        }
        let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
        if magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad magic 0x{:08x}, expected 0x{:08x}", magic, MAGIC),
            ));
        }
        let type_raw = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        if len > MAX_PAYLOAD_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("payload len {} exceeds cap {}", len, MAX_PAYLOAD_LEN),
            ));
        }
        let mut payload = vec![0u8; len];
        if len > 0 {
            r.read_exact(&mut payload)?;
        }
        Ok(Some(match MsgType::from_u32(type_raw) {
            Some(msg_type) => Next::Frame(Frame { msg_type, payload }),
            None => Next::Unknown(type_raw),
        }))
    }
}

enum ReadEnd {
    Full,
    Eof,
}

fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<ReadEnd> {
    let mut off = 0;
    while off < buf.len() {
        match r.read(&mut buf[off..]) {
            Ok(0) => {
                if off == 0 {
                    return Ok(ReadEnd::Eof);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF mid-frame",
                ));
            }
            Ok(n) => off += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(ReadEnd::Full)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell_proto::{decode_ping, encode_ping};
    use std::io::Cursor;

    fn unknown_then_ping() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC.to_le_bytes());
        buf.extend_from_slice(&999u32.to_le_bytes());
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        Frame::new(MsgType::Ping, encode_ping(7))
            .write_to(&mut buf)
            .unwrap();
        buf
    }

    #[test]
    fn read_next_returns_an_unknown_frame_without_reading_past_it() {
        let buf = unknown_then_ping();
        let mut cur = Cursor::new(buf);
        assert!(matches!(
            Frame::read_next(&mut cur).unwrap(),
            Some(Next::Unknown(999))
        ));
        assert_eq!(
            cur.position(),
            HEADER_LEN as u64 + 3,
            "read past the unknown frame"
        );
        match Frame::read_next(&mut cur).unwrap() {
            Some(Next::Frame(f)) => assert_eq!(decode_ping(&f.payload).unwrap(), 7),
            other => panic!("not the ping: {other:?}"),
        }
        assert!(Frame::read_next(&mut cur).unwrap().is_none());
    }
}
