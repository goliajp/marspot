//! `SurfaceAttach`: the two surfaces of a window and its size.

use std::io;

/// Resize payload layout:
///
/// ```text
/// [new_surface_id : u32 LE]
/// [w_phys         : f64 LE]
/// [h_phys         : f64 LE]
/// [scale          : f64 LE]
/// ```
///
/// The shell creates a fresh IOSurface at the new dimensions, then
/// sends this frame.  The core looks the surface up, rebuilds its
/// render target and layout, and acks with `SurfaceReady(id)`.
/// SURFACE_ATTACH payload (32 bytes LE):
///   front_id u32, back_id u32, w_phys f64, h_phys f64, scale f64.
/// Introduced at PROTO_VERSION=2 for double-buffer IOSurface.
pub fn encode_surface_attach(
    front_id: u32,
    back_id: u32,
    w_phys: f64,
    h_phys: f64,
    scale: f64,
) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + 4 + 8 + 8 + 8);
    v.extend_from_slice(&front_id.to_le_bytes());
    v.extend_from_slice(&back_id.to_le_bytes());
    v.extend_from_slice(&w_phys.to_le_bytes());
    v.extend_from_slice(&h_phys.to_le_bytes());
    v.extend_from_slice(&scale.to_le_bytes());
    v
}

pub fn decode_surface_attach(payload: &[u8]) -> io::Result<(u32, u32, f64, f64, f64)> {
    if payload.len() != 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SURFACE_ATTACH payload != 32 bytes",
        ));
    }
    Ok((
        u32::from_le_bytes(payload[0..4].try_into().unwrap()),
        u32::from_le_bytes(payload[4..8].try_into().unwrap()),
        f64::from_le_bytes(payload[8..16].try_into().unwrap()),
        f64::from_le_bytes(payload[16..24].try_into().unwrap()),
        f64::from_le_bytes(payload[24..32].try_into().unwrap()),
    ))
}
