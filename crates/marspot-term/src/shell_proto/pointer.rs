//! The `PanePointerInput` payload: a pointer event for the pane's
//! program, described rather than encoded.
//!
//! Bytes encoded by the core are encoded against the core's copy of the
//! terminal modes, which is a frame behind the session's at best and,
//! while the session holds its grid, as far behind as the hold is long.
//! A report encoded against a mode the program has already turned off
//! is typed into whatever reads the tty next, usually a shell. So the
//! session encodes, at the moment it writes, from its own modes, and
//! writes nothing when the event is not one the program asked for.

use std::io;

/// What happened.  The mode a report needs follows from this and the
/// button: see `marspot_session::pointer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PointerKind {
    /// A button went down: `button` 0 left, 1 middle, 2 right.
    Press = 0,
    /// A button came up.
    Release = 1,
    /// The pointer entered another cell; `button` is the one held, or 3
    /// for none.
    Motion = 2,
    /// `count` wheel ticks; `button` 64 up, 65 down.
    Wheel = 3,
    /// The wheel as `count` arrow keys, for a full-screen program that
    /// asked for no mouse reports; `button` 64 up, 65 down.
    Arrows = 4,
    /// Keys a plugin bound to this pane's wheel, carried in `keys`.
    /// Written only while a program other than the shell has the tty.
    ProgramKeys = 5,
}

impl PointerKind {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Press,
            1 => Self::Release,
            2 => Self::Motion,
            3 => Self::Wheel,
            4 => Self::Arrows,
            5 => Self::ProgramKeys,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointerInput {
    pub kind: PointerKind,
    pub button: u8,
    pub count: u16,
    /// 1-based cell column and row.
    pub col: u32,
    pub row: u32,
    pub keys: Vec<u8>,
}

const FIXED: usize = 8 + 1 + 1 + 2 + 4 + 4 + 4;

/// `session_id u64, kind u8, button u8, count u16, col u32, row u32,
/// keys_len u32, keys`, little endian.
pub fn encode_pane_pointer_input(session_id: u64, ev: &PointerInput) -> Vec<u8> {
    let mut v = Vec::with_capacity(FIXED + ev.keys.len());
    v.extend_from_slice(&session_id.to_le_bytes());
    v.push(ev.kind as u8);
    v.push(ev.button);
    v.extend_from_slice(&ev.count.to_le_bytes());
    v.extend_from_slice(&ev.col.to_le_bytes());
    v.extend_from_slice(&ev.row.to_le_bytes());
    v.extend_from_slice(&(ev.keys.len() as u32).to_le_bytes());
    v.extend_from_slice(&ev.keys);
    v
}

/// Bytes after the keys are ignored, so a later sender can append
/// fields.  A kind this reader does not know is refused: it cannot
/// know what mode such an event needs, and writing nothing is the
/// safe reading.
pub fn decode_pane_pointer_input(p: &[u8]) -> io::Result<(u64, PointerInput)> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, format!("PanePointerInput {m}"));
    if p.len() < FIXED {
        return Err(bad("too short"));
    }
    let sid = u64::from_le_bytes(p[0..8].try_into().unwrap());
    let kind = PointerKind::from_u8(p[8]).ok_or_else(|| bad("kind unknown"))?;
    let button = p[9];
    let count = u16::from_le_bytes(p[10..12].try_into().unwrap());
    let col = u32::from_le_bytes(p[12..16].try_into().unwrap());
    let row = u32::from_le_bytes(p[16..20].try_into().unwrap());
    let keys_len = u32::from_le_bytes(p[20..24].try_into().unwrap()) as usize;
    let keys = p
        .get(FIXED..FIXED.saturating_add(keys_len))
        .ok_or_else(|| bad("keys truncated"))?;
    Ok((
        sid,
        PointerInput {
            kind,
            button,
            count,
            col,
            row,
            keys: keys.to_vec(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: PointerKind, keys: &[u8]) -> PointerInput {
        PointerInput {
            kind,
            button: 3,
            count: 2,
            col: 300,
            row: 70000,
            keys: keys.to_vec(),
        }
    }

    #[test]
    fn every_kind_round_trips() {
        for k in 0..=5u8 {
            let e = ev(
                PointerKind::from_u8(k).unwrap(),
                if k == 5 { b"\x1b[5~" } else { b"" },
            );
            assert_eq!(
                decode_pane_pointer_input(&encode_pane_pointer_input(42, &e)).unwrap(),
                (42, e)
            );
        }
    }

    #[test]
    fn an_unknown_kind_and_a_short_payload_are_refused_and_a_longer_one_is_read() {
        let mut p = encode_pane_pointer_input(1, &ev(PointerKind::Wheel, b""));
        let mut unknown = p.clone();
        unknown[8] = 6;
        assert!(decode_pane_pointer_input(&unknown).is_err());
        assert!(decode_pane_pointer_input(&p[..FIXED - 1]).is_err());
        let mut keys = encode_pane_pointer_input(1, &ev(PointerKind::ProgramKeys, b"abc"));
        keys.pop();
        assert!(decode_pane_pointer_input(&keys).is_err(), "keys cut short");
        p.extend_from_slice(&[9, 9, 9]);
        assert_eq!(
            decode_pane_pointer_input(&p).unwrap().1,
            ev(PointerKind::Wheel, b"")
        );
    }
}
