//! The handshake and liveness payloads, and what a reader of a
//! session's frames says it reads.

use std::io;

/// HELLO payload: protocol version the sender speaks (u32).
pub fn encode_hello(version: u32) -> Vec<u8> {
    version.to_le_bytes().to_vec()
}
pub fn decode_hello(payload: &[u8]) -> io::Result<u32> {
    if payload.len() != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "HELLO payload != 4 bytes",
        ));
    }
    Ok(u32::from_le_bytes(payload[0..4].try_into().unwrap()))
}

/// HELLO_ACK: same shape as HELLO — version the receiver agrees on
/// (echoed back, so the sender knows both sides are aligned).
pub fn encode_hello_ack(version: u32) -> Vec<u8> {
    version.to_le_bytes().to_vec()
}
pub fn decode_hello_ack(payload: &[u8]) -> io::Result<u32> {
    decode_hello(payload)
}

/// PING / PONG payload: u32 LE nonce.  Shell rotates the nonce so it
/// can ignore Pongs from a previous Ping that arrived after a timeout.
pub fn encode_ping(nonce: u32) -> Vec<u8> {
    nonce.to_le_bytes().to_vec()
}
pub fn decode_ping(payload: &[u8]) -> io::Result<u32> {
    if payload.len() != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PING payload != 4 bytes",
        ));
    }
    Ok(u32::from_le_bytes(payload[0..4].try_into().unwrap()))
}
pub fn encode_pong(nonce: u32) -> Vec<u8> {
    nonce.to_le_bytes().to_vec()
}
pub fn decode_pong(payload: &[u8]) -> io::Result<u32> {
    decode_ping(payload)
}

/// SESSION_FRAME_CELLS (core → session): the newest grid-region frame
/// layout the core reads, as the header's `version` names it: u32.
/// Sent first on every connection.  A session shows a core nothing but
/// twenty-byte frames until it has this from that core, and a session
/// from before the message drops it unread.
pub fn encode_frame_cells(version: u32) -> Vec<u8> {
    version.to_le_bytes().to_vec()
}
/// `None` for a payload that is not four bytes: read as a core that
/// reads nothing new.
pub fn decode_frame_cells(payload: &[u8]) -> Option<u32> {
    payload.try_into().ok().map(u32::from_le_bytes)
}
