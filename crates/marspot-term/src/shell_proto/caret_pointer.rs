//! Where the text caret is, for the input method's candidate window,
//! and the pointer's shape over a web pane: both L2 → L1.

use std::io;

use super::trailing_window_id;

/// CaretRect payload: `[present: u8]` then, when present == 1,
/// `4 × f64 LE` (x, y, w, h) in view-local physical pixels with a
/// top-left origin — the exact tuple `set_caret_rect_phys` takes.
/// The trailing window id sits after whatever the variant carries —
/// offset 1 for `None`, offset 33 for `Some` — because it has to go
/// *after* the existing bytes to stay invisible to an old reader.
pub fn encode_caret_rect(rect: Option<(f64, f64, f64, f64)>, window_id: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(37);
    match rect {
        None => out.push(0),
        Some((x, y, w, h)) => {
            out.push(1);
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
            out.extend_from_slice(&w.to_le_bytes());
            out.extend_from_slice(&h.to_le_bytes());
        }
    }
    out.extend_from_slice(&window_id.to_le_bytes());
    out
}

pub fn decode_caret_rect(payload: &[u8]) -> io::Result<(Option<(f64, f64, f64, f64)>, u32)> {
    if payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "caret_rect payload empty",
        ));
    }
    if payload[0] == 0 {
        return Ok((None, trailing_window_id(payload, 1)));
    }
    if payload.len() < 33 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "caret_rect payload < 33 bytes",
        ));
    }
    let f = |i: usize| f64::from_le_bytes(payload[i..i + 8].try_into().unwrap());
    Ok((
        Some((f(1), f(9), f(17), f(25))),
        trailing_window_id(payload, 33),
    ))
}

/// The pointer's shape: what is under it in a web pane's page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerShape {
    Arrow = 0,
    /// A link
    Hand = 1,
    /// Text that can be typed into
    IBeam = 2,
}

/// `PointerShape` payload: `[shape: u8, window_id: u32 LE]`.
pub fn encode_pointer_shape(shape: PointerShape, window_id: u32) -> Vec<u8> {
    let mut out = vec![shape as u8];
    out.extend_from_slice(&window_id.to_le_bytes());
    out
}

pub fn decode_pointer_shape(payload: &[u8]) -> io::Result<(PointerShape, u32)> {
    let shape = match payload.first() {
        Some(0) => PointerShape::Arrow,
        Some(1) => PointerShape::Hand,
        Some(2) => PointerShape::IBeam,
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "pointer_shape payload")),
    };
    Ok((shape, trailing_window_id(payload, 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_shape_round_trips_and_refuses_unknown_shapes() {
        for s in [PointerShape::Arrow, PointerShape::Hand, PointerShape::IBeam] {
            assert_eq!(decode_pointer_shape(&encode_pointer_shape(s, 7)).unwrap(), (s, 7));
        }
        assert!(decode_pointer_shape(&[3, 0, 0, 0, 0]).is_err());
        assert!(decode_pointer_shape(&[]).is_err());
    }
}
