//! Writing frames from a borrowed payload.

use std::io::{self, Write};

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
/// `Frame::new(msg_type, payload.to_vec()).write_to(w)` writes.
pub fn write_frame<W: Write>(w: &mut W, msg_type: MsgType, payload: &[u8]) -> io::Result<usize> {
    w.write_all(&frame_header(msg_type, payload.len()))?;
    if !payload.is_empty() {
        w.write_all(payload)?;
    }
    Ok(HEADER_LEN + payload.len())
}
