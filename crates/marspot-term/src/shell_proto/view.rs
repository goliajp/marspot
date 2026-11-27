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

/// A jump the core asks for: which way, and the core's count of the
/// scrolls it has asked for, which the answer carries back so that an
/// answer overtaken by a later scroll is known for one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromptJump {
    pub dir: PromptDir,
    pub seq: u32,
}

/// PromptJump payload: `dir u8` (0 back, 1 forward), `seq u32 LE`,
/// written into `out`, which is the writer's queue: a key press
/// allocates nothing.
pub fn encode_prompt_jump(jump: PromptJump, out: &mut Vec<u8>) {
    out.push(match jump.dir {
        PromptDir::Back => 0,
        PromptDir::Forward => 1,
    });
    out.extend_from_slice(&jump.seq.to_le_bytes());
}

/// A core from before the count sent the direction alone: its `seq`
/// reads as 0, which that core never looks at.
pub fn decode_prompt_jump(payload: &[u8]) -> io::Result<PromptJump> {
    let dir = match payload.first() {
        Some(0) => PromptDir::Back,
        Some(1) => PromptDir::Forward,
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "prompt_jump: no direction")),
    };
    let seq = payload.get(1..5).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    Ok(PromptJump { dir, seq })
}

/// Where a jump went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromptJumped {
    /// The offset the session publishes at after the jump.
    pub view_offset: u16,
    /// The row of that view the prompt is on: 0 for a prompt in the
    /// history, its row on the live screen when the jump went live.
    pub row: u16,
    /// The `seq` of the jump this answers; `None` from a session from
    /// before the count.
    pub seq: Option<u32>,
}

/// PromptJumped payload: `view_offset u16 LE`, `row u16 LE`, `seq u32
/// LE`.  A core from before the row reads the offset alone.
pub fn encode_prompt_jumped(j: PromptJumped) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&j.view_offset.to_le_bytes());
    out.extend_from_slice(&j.row.to_le_bytes());
    out.extend_from_slice(&j.seq.unwrap_or(0).to_le_bytes());
    out
}

/// A session from before the row and the count sent the offset alone:
/// row 0, no count.  Bytes after the count are a later build's.
pub fn decode_prompt_jumped(payload: &[u8]) -> io::Result<PromptJumped> {
    let b = payload.first_chunk::<2>().ok_or_else(|| short("prompt_jumped", 2))?;
    let row = payload.get(2..4).map_or(0, |r| u16::from_le_bytes([r[0], r[1]]));
    let seq = payload.get(4..8).map(|g| u32::from_le_bytes([g[0], g[1], g[2], g[3]]));
    Ok(PromptJumped { view_offset: u16::from_le_bytes(*b), row, seq })
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
            let jump = PromptJump { dir, seq: 0x0102_0304 };
            let mut out = Vec::new();
            encode_prompt_jump(jump, &mut out);
            assert_eq!(decode_prompt_jump(&out).unwrap(), jump);
        }
        assert_eq!(decode_prompt_jump(&[1]).unwrap(), PromptJump { dir: PromptDir::Forward, seq: 0 }, "an older core");
        assert!(decode_prompt_jump(&[]).is_err());
        assert!(decode_prompt_jump(&[2]).is_err(), "a direction this build does not know is no jump");
        let j = PromptJumped { view_offset: 4096, row: 7, seq: Some(9) };
        assert_eq!(decode_prompt_jumped(&encode_prompt_jumped(j)).unwrap(), j);
        let mut later = encode_prompt_jumped(j);
        later.push(0xff);
        assert_eq!(decode_prompt_jumped(&later).unwrap(), j, "a later build's bytes after it are skipped");
        assert_eq!(
            decode_prompt_jumped(&[7, 0]).unwrap(),
            PromptJumped { view_offset: 7, row: 0, seq: None },
            "an older session's offset alone"
        );
        assert!(decode_prompt_jumped(&[7]).is_err());
        // the numbers are part of the wire and do not move
        for (t, n) in [(MsgType::PromptJump, 126), (MsgType::PromptJumped, 127)] {
            assert_eq!(t as u32, n);
            assert_eq!(MsgType::from_u32(n), Some(t));
        }
    }
}
