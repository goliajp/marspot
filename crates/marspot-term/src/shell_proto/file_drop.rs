//! The `FileDrop` payload: where files were dropped and their paths.

use std::io;

use super::trailing_window_id;

/// Encode a `FileDrop` payload: drop point (physical px, top-left
/// origin) + the dropped paths.  Layout: `x f64 LE, y f64 LE,
/// count u16 LE`, then per path `len u16 LE + UTF-8 bytes`.
pub fn encode_file_drop(x: f64, y: f64, paths: &[String], window_id: u32) -> Vec<u8> {
    let body: usize = paths.iter().map(|p| 2 + p.len()).sum();
    let mut out = Vec::with_capacity(18 + body + 4);
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out.extend_from_slice(&(paths.len() as u16).to_le_bytes());
    for p in paths {
        out.extend_from_slice(&(p.len() as u16).to_le_bytes());
        out.extend_from_slice(p.as_bytes());
    }
    out.extend_from_slice(&window_id.to_le_bytes());
    out
}

pub fn decode_file_drop(payload: &[u8]) -> io::Result<(f64, f64, Vec<String>, u32)> {
    if payload.len() < 18 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file-drop payload < 18 bytes",
        ));
    }
    let x = f64::from_le_bytes(payload[0..8].try_into().unwrap());
    let y = f64::from_le_bytes(payload[8..16].try_into().unwrap());
    let count = u16::from_le_bytes(payload[16..18].try_into().unwrap()) as usize;
    let mut paths = Vec::with_capacity(count);
    let mut off = 18usize;
    for _ in 0..count {
        if payload.len() < off + 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file-drop path header truncated",
            ));
        }
        let len = u16::from_le_bytes(payload[off..off + 2].try_into().unwrap()) as usize;
        off += 2;
        if payload.len() < off + len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file-drop path truncated",
            ));
        }
        let s = std::str::from_utf8(&payload[off..off + len])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        paths.push(s.to_string());
        off += len;
    }
    Ok((x, y, paths, trailing_window_id(payload, off)))
}
