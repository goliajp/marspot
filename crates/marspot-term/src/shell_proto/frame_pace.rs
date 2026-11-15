//! Frames a window is shown: which surface is ready, and when the one
//! before it has left the screen.
//!
//! A shell that puts the core's surface on the window itself, with no
//! copy of its own in between, has the window server reading that
//! surface whenever it composites.  The core may then paint a surface
//! only once it is off the screen, and the shell is the one that knows:
//!
//! - `FramePace` (shell → core, empty, sent once before `Hello`): "I
//!   show your surfaces directly and will say when each has been
//!   shown."  A core that knows the frame answers `FramePaceAck`
//!   (empty) before it answers the `Hello`, so by the `HelloAck` the
//!   shell knows which kind of core it has.  A core that does not know
//!   it skips it, as every reader skips an unknown type, and the shell
//!   copies its frames as it always did.
//! - `FrameShown` (shell → core): the surface named was put on the
//!   window, and the one it replaced is off the screen as of a display
//!   refresh later.  From `SurfaceReady(id)` until `FrameShown` names
//!   that id the core draws nothing for the window; what changes
//!   meanwhile is drawn once, afterwards.  A window nobody can see has
//!   no refresh, so the frame arrives when it can be seen again.
//!
//! A core never told `FramePace` paces itself on a clock, as before.

use std::io;

/// SurfaceReady payload: the IOSurface ID the core has just rendered
/// to (u32 LE).
pub fn encode_surface_ready(surface_id: u32) -> Vec<u8> {
    surface_id.to_le_bytes().to_vec()
}

pub fn decode_surface_ready(payload: &[u8]) -> io::Result<u32> {
    if payload.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "surface_ready payload < 4 bytes",
        ));
    }
    Ok(u32::from_le_bytes(payload[0..4].try_into().unwrap()))
}

/// FrameShown payload: `window_id u32 LE`, `surface_id u32 LE`.
pub fn encode_frame_shown(window_id: u32, surface_id: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&window_id.to_le_bytes());
    out.extend_from_slice(&surface_id.to_le_bytes());
    out
}

pub fn decode_frame_shown(payload: &[u8]) -> io::Result<(u32, u32)> {
    if payload.len() < 8 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame_shown payload < 8 bytes"));
    }
    Ok((
        u32::from_le_bytes(payload[0..4].try_into().unwrap()),
        u32::from_le_bytes(payload[4..8].try_into().unwrap()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shown_frame_names_its_window_and_its_surface() {
        let p = encode_frame_shown(7, 0xdead_beef);
        assert_eq!(decode_frame_shown(&p).unwrap(), (7, 0xdead_beef));
        assert!(decode_frame_shown(&p[..7]).is_err());
    }
}
