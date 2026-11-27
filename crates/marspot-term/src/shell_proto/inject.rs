//! The payloads that put input into a pane: `InjectInput`,
//! `PaneInjectPaste` and `ProgramInput`.

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

/// PaneInjectPaste payload: `session_id u64 LE, len u32 LE, utf8`.
pub fn encode_pane_inject_paste(session_id: u64, text: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(12 + text.len());
    v.extend_from_slice(&session_id.to_le_bytes());
    v.extend_from_slice(&(text.len() as u32).to_le_bytes());
    v.extend_from_slice(text.as_bytes());
    v
}

pub fn decode_pane_inject_paste(payload: &[u8]) -> io::Result<(u64, String)> {
    if payload.len() < 12 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PaneInjectPaste payload too short",
        ));
    }
    let sid = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let n = u32::from_le_bytes(payload[8..12].try_into().unwrap()) as usize;
    if payload.len() < 12 + n {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PaneInjectPaste payload truncated before body",
        ));
    }
    Ok((
        sid,
        String::from_utf8_lossy(&payload[12..12 + n]).into_owned(),
    ))
}

/// A `ProgramInput` payload: what a program outside marspot asked to
/// have typed or pasted into a pane.
#[derive(Debug, PartialEq, Eq)]
pub struct ProgramInput {
    pub session_id: u64,
    pub paste: bool,
    pub bytes: Vec<u8>,
}

/// u64 LE session_id + u8 paste + u32 LE len + bytes.
pub fn encode_program_input(session_id: u64, paste: bool, bytes: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(13 + bytes.len());
    v.extend_from_slice(&session_id.to_le_bytes());
    v.push(u8::from(paste));
    v.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    v.extend_from_slice(bytes);
    v
}

pub fn decode_program_input(payload: &[u8]) -> io::Result<ProgramInput> {
    let bad = |why: &str| io::Error::new(io::ErrorKind::InvalidData, format!("ProgramInput {why}"));
    let (head, rest) = payload.split_at_checked(13).ok_or_else(|| bad("too short"))?;
    let session_id = u64::from_le_bytes(head[0..8].try_into().unwrap());
    let len = u32::from_le_bytes(head[9..13].try_into().unwrap()) as usize;
    // what follows the bytes is a later version's and is skipped
    let bytes = rest.get(..len).ok_or_else(|| bad("truncated before its bytes"))?.to_vec();
    Ok(ProgramInput { session_id, paste: head[8] != 0, bytes })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_input_round_trips_and_rejects_a_short_payload() {
        let v = encode_program_input(7, true, b"/model\r");
        assert_eq!(decode_program_input(&v).unwrap(), ProgramInput { session_id: 7, paste: true, bytes: b"/model\r".to_vec() });
        let mut longer = encode_program_input(8, false, b"x");
        longer.push(9);
        assert_eq!(decode_program_input(&longer).unwrap().bytes, b"x");
        assert!(decode_program_input(&v[..12]).is_err());
        assert!(decode_program_input(&v[..15]).is_err());
    }
}
