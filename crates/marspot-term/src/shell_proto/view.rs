//! What the core asks of a session's view: its size, how far back it is
//! scrolled, and a move to the prompt before or after it.

use std::io;

/// GridResize payload: `cols: u16 LE, rows: u16 LE` — the cell-grid
/// dims L2 wants this L3 session resized to (the in-view window L3
/// publishes into the shm framebuffer follows these dims).
pub fn encode_grid_resize(cols: u16, rows: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(4);
    out.extend_from_slice(&cols.to_le_bytes());
    out.extend_from_slice(&rows.to_le_bytes());
    out
}

pub fn decode_grid_resize(payload: &[u8]) -> io::Result<(u16, u16)> {
    let b = payload.first_chunk::<4>().ok_or_else(|| short("grid_resize", 4))?;
    Ok((u16::from_le_bytes([b[0], b[1]]), u16::from_le_bytes([b[2], b[3]])))
}

/// GridScroll payload: `view_offset: u16 LE` — rows up from the live
/// tail that L3 should publish into the framebuffer (0 = live).
pub fn encode_grid_scroll(view_offset: u16) -> Vec<u8> {
    view_offset.to_le_bytes().to_vec()
}

pub fn decode_grid_scroll(payload: &[u8]) -> io::Result<u16> {
    let b = payload.first_chunk::<2>().ok_or_else(|| short("grid_scroll", 2))?;
    Ok(u16::from_le_bytes(*b))
}

/// Which way a `PromptJump` goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptDir {
    /// To the prompt above the line at the top of the view.
    Back,
    /// To the prompt below it, or to the live screen.
    Forward,
}

/// PromptJump payload: `dir u8` (0 back, 1 forward), written into
/// `out`, which is the writer's queue: a key press allocates nothing.
pub fn encode_prompt_jump(dir: PromptDir, out: &mut Vec<u8>) {
    out.push(match dir {
        PromptDir::Back => 0,
        PromptDir::Forward => 1,
    });
}

pub fn decode_prompt_jump(payload: &[u8]) -> io::Result<PromptDir> {
    match payload.first() {
        Some(0) => Ok(PromptDir::Back),
        Some(1) => Ok(PromptDir::Forward),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "prompt_jump: no direction")),
    }
}

/// PromptJumped payload: `view_offset u16 LE`, the offset the session
/// publishes at after the jump.  Bytes after it are a later build's.
pub fn encode_prompt_jumped(view_offset: u16) -> Vec<u8> {
    view_offset.to_le_bytes().to_vec()
}

pub fn decode_prompt_jumped(payload: &[u8]) -> io::Result<u16> {
    let b = payload.first_chunk::<2>().ok_or_else(|| short("prompt_jumped", 2))?;
    Ok(u16::from_le_bytes(*b))
}

fn short(what: &str, need: usize) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{what} payload < {need} bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell_proto::MsgType;

    #[test]
    fn prompt_jump_roundtrip() {
        for dir in [PromptDir::Back, PromptDir::Forward] {
            let mut out = Vec::new();
            encode_prompt_jump(dir, &mut out);
            assert_eq!(decode_prompt_jump(&out).unwrap(), dir);
        }
        assert!(decode_prompt_jump(&[]).is_err());
        assert!(decode_prompt_jump(&[2]).is_err(), "a direction this build does not know is no jump");
        assert_eq!(decode_prompt_jumped(&encode_prompt_jumped(4096)).unwrap(), 4096);
        assert_eq!(decode_prompt_jumped(&[7, 0, 0xff]).unwrap(), 7, "a later build's bytes after it are skipped");
        assert!(decode_prompt_jumped(&[7]).is_err());
        // the numbers are part of the wire and do not move
        for (t, n) in [(MsgType::PromptJump, 126), (MsgType::PromptJumped, 127)] {
            assert_eq!(t as u32, n);
            assert_eq!(MsgType::from_u32(n), Some(t));
        }
    }
}
