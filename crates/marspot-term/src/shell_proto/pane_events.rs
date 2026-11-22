//! Payloads of the small per-pane messages: what a pane's session
//! reports about it, and what the core tells a session about its pane.

use std::io;

/// What a session saw happen in its pane, sent as `PaneAwake`.
///
/// The session sends it to its core, and the core passes it on to the
/// shell unchanged but for the id, which the core fills in from the
/// connection it came on.  A reader from before the message skips it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneAwake {
    pub session_id: u64,
    /// The pane printed after a quiet spell, or is still printing.
    pub output: bool,
    /// The terminal's foreground process group is another one than
    /// when the last of these was sent.
    pub foreground: bool,
    /// The pane's shell is that foreground group.
    pub shell_in_front: bool,
    /// The foreground process group when this was sent; 0 when the
    /// terminal has none or it could not be read.
    pub fg_pgid: i32,
}

const OUTPUT: u8 = 1;
const FOREGROUND: u8 = 2;
const SHELL_IN_FRONT: u8 = 4;
const PANE_AWAKE_LEN: usize = 13;

/// `session_id u64 LE, what u8 (bits: output, foreground, shell in
/// front), fg_pgid i32 LE`.
pub fn encode_pane_awake(a: &PaneAwake) -> Vec<u8> {
    let mut v = Vec::with_capacity(PANE_AWAKE_LEN);
    v.extend_from_slice(&a.session_id.to_le_bytes());
    let what = (a.output as u8 * OUTPUT) | (a.foreground as u8 * FOREGROUND) | (a.shell_in_front as u8 * SHELL_IN_FRONT);
    v.push(what);
    v.extend_from_slice(&a.fg_pgid.to_le_bytes());
    v
}

/// Bytes past the ones known here are a later build's additions, and
/// are ignored.
pub fn decode_pane_awake(payload: &[u8]) -> io::Result<PaneAwake> {
    if payload.len() < PANE_AWAKE_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "pane_awake payload < 13 bytes"));
    }
    let what = payload[8];
    Ok(PaneAwake {
        session_id: u64::from_le_bytes(payload[0..8].try_into().unwrap()),
        output: what & OUTPUT != 0,
        foreground: what & FOREGROUND != 0,
        shell_in_front: what & SHELL_IN_FRONT != 0,
        fg_pgid: i32::from_le_bytes(payload[9..13].try_into().unwrap()),
    })
}

/// PaneResetMouseReporting payload: `session_id u64 LE`.
pub fn encode_pane_reset_mouse_reporting(session_id: u64) -> Vec<u8> {
    session_id.to_le_bytes().to_vec()
}

pub fn decode_pane_reset_mouse_reporting(payload: &[u8]) -> io::Result<u64> {
    if payload.len() != 8 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "pane_reset_mouse_reporting payload != 8 bytes"));
    }
    Ok(u64::from_le_bytes(payload.try_into().unwrap()))
}

/// PaneTerminalFocus payload: `session_id u64 LE, focused u8`.
pub fn encode_pane_terminal_focus(session_id: u64, focused: bool) -> Vec<u8> {
    let mut v = Vec::with_capacity(9);
    v.extend_from_slice(&session_id.to_le_bytes());
    v.push(focused as u8);
    v
}

pub fn decode_pane_terminal_focus(payload: &[u8]) -> io::Result<(u64, bool)> {
    if payload.len() != 9 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "pane_terminal_focus payload != 9 bytes"));
    }
    Ok((u64::from_le_bytes(payload[0..8].try_into().unwrap()), payload[8] != 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell_proto::{Frame, HEADER_LEN, MsgType, Next, decode_ping, encode_ping};
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    #[test]
    fn pane_awake_round_trips_and_a_longer_one_still_decodes() {
        let a = PaneAwake { session_id: 0xDEAD_BEEF_0042, output: true, foreground: false, shell_in_front: true, fg_pgid: 4321 };
        let mut p = encode_pane_awake(&a);
        assert_eq!(p.len(), 13);
        assert_eq!(decode_pane_awake(&p).unwrap(), a);
        p.extend_from_slice(&[9, 9, 9]);
        assert_eq!(decode_pane_awake(&p).unwrap(), a, "a later build's extra bytes are ignored");
        assert!(decode_pane_awake(&p[..12]).is_err());
        assert_eq!(MsgType::PaneAwake as u32, 124);
    }

    /// The control loop of a build from before `PaneAwake`, as both the
    /// shell's and the core's are: read with `read_from`, act on the
    /// types it knows.  Its `from_u32` has no 124, so the frame comes
    /// to it as `Next::Unknown`; everything else about the frame on the
    /// wire is what this build writes.
    fn old_reader_loop(mut stream: UnixStream) -> Vec<u32> {
        let mut seen = Vec::new();
        loop {
            let next = match Frame::read_next(&mut stream) {
                Ok(Some(Next::Frame(f))) if f.msg_type == MsgType::PaneAwake => Next::Unknown(f.msg_type as u32),
                Ok(Some(n)) => n,
                Ok(None) | Err(_) => return seen,
            };
            match next {
                Next::Frame(f) => seen.push(decode_ping(&f.payload).unwrap()),
                Next::Unknown(_) => continue,
            }
        }
    }

    #[test]
    fn a_reader_from_before_pane_awake_skips_it_and_keeps_the_channel() {
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        let reader = std::thread::spawn(move || old_reader_loop(theirs));
        let awake = PaneAwake { session_id: 7, output: true, foreground: true, shell_in_front: false, fg_pgid: 99 };
        for i in 0..3 {
            Frame::new(MsgType::PaneAwake, encode_pane_awake(&awake)).write_to(&mut ours).unwrap();
            Frame::new(MsgType::Ping, encode_ping(i)).write_to(&mut ours).unwrap();
        }
        drop(ours);
        assert_eq!(reader.join().unwrap(), vec![0, 1, 2], "every frame after each PaneAwake was read");
    }

    /// What lets an old reader skip it: the header's length covers the
    /// whole payload, so the next read starts at the next frame.
    #[test]
    fn the_frame_is_one_header_and_its_payload() {
        let mut buf = Vec::new();
        let awake = PaneAwake { session_id: 1, output: false, foreground: true, shell_in_front: true, fg_pgid: 5 };
        Frame::new(MsgType::PaneAwake, encode_pane_awake(&awake)).write_to(&mut buf).unwrap();
        assert_eq!(buf.len(), HEADER_LEN + 13);
        assert_eq!(u32::from_le_bytes(buf[4..8].try_into().unwrap()), 124);
        assert_eq!(u32::from_le_bytes(buf[8..12].try_into().unwrap()), 13);
        let mut rest = Vec::new();
        let mut cur = std::io::Cursor::new(buf);
        assert!(matches!(Frame::read_next(&mut cur).unwrap(), Some(Next::Frame(_))));
        cur.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
    }

    #[test]
    fn pane_terminal_focus_roundtrip() {
        for focused in [true, false] {
            let p = encode_pane_terminal_focus(7, focused);
            assert_eq!(decode_pane_terminal_focus(&p).unwrap(), (7, focused));
        }
        assert!(decode_pane_terminal_focus(&[0u8; 8]).is_err());
        assert_eq!(MsgType::from_u32(84), Some(MsgType::PaneTerminalFocus));
    }

    #[test]
    fn pane_reset_mouse_reporting_roundtrip() {
        let p = encode_pane_reset_mouse_reporting(0xDEAD_BEEF_1234_5678);
        assert_eq!(decode_pane_reset_mouse_reporting(&p).unwrap(), 0xDEAD_BEEF_1234_5678);
        // this frame carries nothing but the id, so a mis-sized one is
        // a bug on the sender, not a version skew
        assert!(decode_pane_reset_mouse_reporting(&p[..7]).is_err());
    }
}
