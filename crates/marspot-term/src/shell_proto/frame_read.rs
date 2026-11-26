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
        let Some((type_raw, len)) = read_header(r)? else { return Ok(None) };
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

/// One frame as [`FrameReader`] lends it out.
#[derive(Debug)]
pub enum Seen<'a> {
    Frame(MsgType, &'a [u8]),
    /// A frame of a type this build does not know, `u32` the type; its
    /// payload has been read and dropped.
    Unknown(u32),
}

/// Reads frames into one buffer it keeps, and lends each payload to the
/// caller for as long as a closure runs.  A reader that sees a frame per
/// keystroke stops allocating once the buffer has reached the size of
/// the frames it carries; a caller that keeps a payload copies it.
#[derive(Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read exactly one frame off `r`, known or not, and hand it to `f`;
    /// `None` at a clean end between two frames.
    pub fn read_next<R: Read, T>(&mut self, r: &mut R, f: impl FnOnce(Seen<'_>) -> T) -> io::Result<Option<T>> {
        let Some((type_raw, len)) = read_header(r)? else { return Ok(None) };
        self.buf.clear();
        self.buf.resize(len, 0);
        r.read_exact(&mut self.buf)?;
        Ok(Some(f(match MsgType::from_u32(type_raw) {
            Some(t) => Seen::Frame(t, &self.buf),
            None => Seen::Unknown(type_raw),
        })))
    }

    /// [`Self::read_next`], skipping frames of types this build does not
    /// know, as [`Frame::read_from`] does.
    pub fn read_known<R: Read, T>(
        &mut self,
        r: &mut R,
        mut f: impl FnMut(MsgType, &[u8]) -> T,
    ) -> io::Result<Option<T>> {
        loop {
            let got = self.read_next(r, |seen| match seen {
                Seen::Frame(t, payload) => Some(f(t, payload)),
                Seen::Unknown(_) => None,
            })?;
            match got {
                None => return Ok(None),
                Some(Some(out)) => return Ok(Some(out)),
                Some(None) => continue,
            }
        }
    }
}

/// The type and payload length of the next frame, checked; `None` at a
/// clean end between two frames.
fn read_header<R: Read>(r: &mut R) -> io::Result<Option<(u32, usize)>> {
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
    Ok(Some((type_raw, len)))
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

    #[test]
    fn a_frame_reader_lends_each_frame_and_skips_unknown_ones() {
        let mut buf = unknown_then_ping();
        Frame::new(MsgType::Ping, encode_ping(9)).write_to(&mut buf).unwrap();
        let mut r = FrameReader::new();
        let mut cur = Cursor::new(buf.clone());
        let first = r.read_next(&mut cur, |s| matches!(s, Seen::Unknown(999))).unwrap();
        assert_eq!(first, Some(true));
        let mut cur = Cursor::new(buf);
        let mut pings = Vec::new();
        while let Some(n) = r.read_known(&mut cur, |t, p| (t, decode_ping(p).unwrap())).unwrap() {
            pings.push(n);
        }
        assert_eq!(pings, [(MsgType::Ping, 7), (MsgType::Ping, 9)]);
    }
}
