//! The `InjectInput` payload.

use std::io;

/// Encode an `InjectInput` payload: u64 LE session_id + u32 LE len +
/// raw bytes.  No bracketed-paste markup — the caller controls every
/// byte that ends up on the PTY.
pub fn encode_inject_input(session_id: u64, bytes: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(12 + bytes.len());
    v.extend_from_slice(&session_id.to_le_bytes());
    v.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    v.extend_from_slice(bytes);
    v
}

pub fn decode_inject_input(payload: &[u8]) -> io::Result<(u64, Vec<u8>)> {
    if payload.len() < 12 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "InjectInput too short",
        ));
    }
    let sid = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let len = u32::from_le_bytes(payload[8..12].try_into().unwrap()) as usize;
    if payload.len() != 12 + len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("InjectInput len {len} but payload total {}", payload.len()),
        ));
    }
    Ok((sid, payload[12..].to_vec()))
}
