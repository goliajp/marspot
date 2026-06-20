//! F3+6 — marspot's L2 / L1 persistence layer.  Single binary file
//! at `~/Library/Caches/marspot/shell-state.bin` snapshots everything
//! a fresh marspot needs to restore the user's last layout:
//!
//! - pane count + order (by index)
//! - per-pane (sid, custom_title, last_cwd) so the cold boot can
//!   reattach surviving L3s in the right slots, or fall back to
//!   spawning a fresh shell in the saved `last_cwd` when the L3
//!   didn't survive (kernel restart, manual `kill -9`, etc.)
//! - grid (cols × rows)
//! - focused pane index
//! - (reserved) window frame + display id — populated by L1 once
//!   the window-state plumbing lands in F3+6.1
//!
//! Format is a plain LE binary header.  Atomic writes via `.tmp` +
//! rename.  Failure to parse / read returns `None` and the boot
//! falls through to the legacy registry-driven reattach.

use std::io::{self, Cursor, Read, Write};
use std::path::PathBuf;

const MAGIC: u32 = 0xA5505010;
const VERSION: u32 = 1;
/// Sanity ceiling — the modal caps grid at 6×6 = 36 + some headroom.
const MAX_PANES: usize = 128;
/// String length cap so a corrupt header can't trigger an OOM
/// allocation.  Real custom_title and cwd are bounded by PATH_MAX
/// (~1 KB) plus user-set labels typically < 64 chars.
const MAX_STR_BYTES: usize = 4096;

#[derive(Debug, Clone, Default)]
pub struct SavedState {
    pub grid_cols: u16,
    pub grid_rows: u16,
    pub focused_idx: u16,
    pub panes: Vec<SavedPane>,
    /// F3+6.1 reserved — None until L1 wires the window frame.
    pub window: Option<SavedWindow>,
}

#[derive(Debug, Clone, Default)]
pub struct SavedPane {
    /// Shelld session id, or 0 for "no surviving sid" (in-process /
    /// pane saved before a sid was assigned).
    pub sid: u64,
    /// User-set custom title.  Empty = no custom title (cwd basename
    /// or ordinal applies at render time).
    pub custom_title: String,
    /// Last-known cwd of the pane's shell.  Used as the spawn cwd
    /// when reattach fails — so the user lands in the same project,
    /// not $HOME.
    pub last_cwd: String,
}

#[derive(Debug, Clone, Default)]
pub struct SavedWindow {
    /// macOS CGDirectDisplayID of the screen the window was on.
    /// 0 = unknown.  L1's restore path looks up the screen by this
    /// id; if the display is no longer connected, falls back to
    /// the main screen.
    pub display_id: u32,
    /// Window frame in screen coordinates (origin = bottom-left
    /// per NSWindow convention).  Logical pt, NOT physical px.
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// F3+6.1 — separate file for the window frame so L1 (marspot-shell,
/// AppKit) can own writes without coordinating with L2 (marspot-core).
/// One file per writer = no atomic-rename race.
const WINDOW_MAGIC: u32 = 0xA5505011;
const WINDOW_VERSION: u32 = 1;

pub fn window_state_file_path() -> PathBuf {
    let base: PathBuf = match std::env::var_os("MARSPOT_STATE_DIR") {
        Some(d) => PathBuf::from(d),
        None => {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join("Library/Caches/marspot")
        }
    };
    base.join("window-state.bin")
}

/// Best-effort read of window frame.  Same fail-soft policy as
/// `read()` — returns None on missing / corrupt / version drift.
pub fn read_window() -> Option<SavedWindow> {
    let body = std::fs::read(window_state_file_path()).ok()?;
    let mut cur = Cursor::new(body.as_slice());
    if read_u32(&mut cur)? != WINDOW_MAGIC { return None; }
    if read_u32(&mut cur)? != WINDOW_VERSION { return None; }
    let display_id = read_u32(&mut cur)?;
    let x = read_f64(&mut cur)?;
    let y = read_f64(&mut cur)?;
    let w = read_f64(&mut cur)?;
    let h = read_f64(&mut cur)?;
    Some(SavedWindow { display_id, x, y, w, h })
}

/// Best-effort atomic write of the window frame.
pub fn write_window(s: &SavedWindow) -> io::Result<()> {
    let path = window_state_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut body: Vec<u8> = Vec::with_capacity(40);
    body.extend_from_slice(&WINDOW_MAGIC.to_le_bytes());
    body.extend_from_slice(&WINDOW_VERSION.to_le_bytes());
    body.extend_from_slice(&s.display_id.to_le_bytes());
    body.extend_from_slice(&s.x.to_le_bytes());
    body.extend_from_slice(&s.y.to_le_bytes());
    body.extend_from_slice(&s.w.to_le_bytes());
    body.extend_from_slice(&s.h.to_le_bytes());
    let mut tmp = path.clone();
    tmp.set_extension("bin.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Resolve `~/Library/Caches/marspot/shell-state.bin`, respecting
/// `MARSPOT_STATE_DIR` for dev sandbox / test paths.
pub fn state_file_path() -> PathBuf {
    let base: PathBuf = match std::env::var_os("MARSPOT_STATE_DIR") {
        Some(d) => PathBuf::from(d),
        None => {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join("Library/Caches/marspot")
        }
    };
    base.join("shell-state.bin")
}

/// Best-effort read.  Returns None on any failure: missing file,
/// magic mismatch, version drift, truncation, OOM-trigger string
/// lengths.  Caller falls through to the legacy boot path.
pub fn read() -> Option<SavedState> {
    let path = state_file_path();
    let body = std::fs::read(&path).ok()?;
    read_from(&body)
}

fn read_from(body: &[u8]) -> Option<SavedState> {
    let mut cur = Cursor::new(body);
    let magic = read_u32(&mut cur)?;
    if magic != MAGIC { return None; }
    let version = read_u32(&mut cur)?;
    if version != VERSION { return None; }
    let grid_cols = read_u16(&mut cur)?;
    let grid_rows = read_u16(&mut cur)?;
    let focused_idx = read_u16(&mut cur)?;
    let pane_count = read_u16(&mut cur)? as usize;
    if pane_count > MAX_PANES { return None; }
    let mut panes = Vec::with_capacity(pane_count);
    for _ in 0..pane_count {
        let sid = read_u64(&mut cur)?;
        let custom_title = read_string(&mut cur)?;
        let last_cwd = read_string(&mut cur)?;
        panes.push(SavedPane { sid, custom_title, last_cwd });
    }
    let has_window = read_u8(&mut cur)?;
    let window = if has_window != 0 {
        let display_id = read_u32(&mut cur)?;
        let x = read_f64(&mut cur)?;
        let y = read_f64(&mut cur)?;
        let w = read_f64(&mut cur)?;
        let h = read_f64(&mut cur)?;
        Some(SavedWindow { display_id, x, y, w, h })
    } else {
        None
    };
    Some(SavedState {
        grid_cols, grid_rows, focused_idx, panes, window,
    })
}

/// Atomic write — `.tmp` + rename.  Failures are logged; not
/// propagated.  Best-effort means a transient I/O hiccup doesn't
/// crash marspot.
pub fn write(s: &SavedState) -> io::Result<()> {
    let path = state_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut body: Vec<u8> = Vec::with_capacity(256);
    body.extend_from_slice(&MAGIC.to_le_bytes());
    body.extend_from_slice(&VERSION.to_le_bytes());
    body.extend_from_slice(&s.grid_cols.to_le_bytes());
    body.extend_from_slice(&s.grid_rows.to_le_bytes());
    body.extend_from_slice(&s.focused_idx.to_le_bytes());
    let n = s.panes.len().min(MAX_PANES) as u16;
    body.extend_from_slice(&n.to_le_bytes());
    for p in s.panes.iter().take(n as usize) {
        body.extend_from_slice(&p.sid.to_le_bytes());
        write_string(&mut body, &p.custom_title);
        write_string(&mut body, &p.last_cwd);
    }
    if let Some(w) = &s.window {
        body.push(1);
        body.extend_from_slice(&w.display_id.to_le_bytes());
        body.extend_from_slice(&w.x.to_le_bytes());
        body.extend_from_slice(&w.y.to_le_bytes());
        body.extend_from_slice(&w.w.to_le_bytes());
        body.extend_from_slice(&w.h.to_le_bytes());
    } else {
        body.push(0);
    }
    let mut tmp = path.clone();
    tmp.set_extension("bin.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn write_string(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let n = (bytes.len().min(MAX_STR_BYTES)) as u16;
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&bytes[..n as usize]);
}

fn read_u8(c: &mut Cursor<&[u8]>) -> Option<u8> {
    let mut b = [0u8; 1]; c.read_exact(&mut b).ok()?; Some(b[0])
}
fn read_u16(c: &mut Cursor<&[u8]>) -> Option<u16> {
    let mut b = [0u8; 2]; c.read_exact(&mut b).ok()?; Some(u16::from_le_bytes(b))
}
fn read_u32(c: &mut Cursor<&[u8]>) -> Option<u32> {
    let mut b = [0u8; 4]; c.read_exact(&mut b).ok()?; Some(u32::from_le_bytes(b))
}
fn read_u64(c: &mut Cursor<&[u8]>) -> Option<u64> {
    let mut b = [0u8; 8]; c.read_exact(&mut b).ok()?; Some(u64::from_le_bytes(b))
}
fn read_f64(c: &mut Cursor<&[u8]>) -> Option<f64> {
    let mut b = [0u8; 8]; c.read_exact(&mut b).ok()?; Some(f64::from_le_bytes(b))
}
fn read_string(c: &mut Cursor<&[u8]>) -> Option<String> {
    let n = read_u16(c)? as usize;
    if n > MAX_STR_BYTES { return None; }
    let mut buf = vec![0u8; n];
    c.read_exact(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_minimal() {
        let s = SavedState {
            grid_cols: 3, grid_rows: 3, focused_idx: 4,
            panes: vec![
                SavedPane { sid: 100, custom_title: "spg".into(), last_cwd: "/Users/x/spg".into() },
                SavedPane { sid: 0, custom_title: "".into(), last_cwd: "/Users/x".into() },
            ],
            window: None,
        };
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC.to_le_bytes());
        body.extend_from_slice(&VERSION.to_le_bytes());
        body.extend_from_slice(&s.grid_cols.to_le_bytes());
        body.extend_from_slice(&s.grid_rows.to_le_bytes());
        body.extend_from_slice(&s.focused_idx.to_le_bytes());
        body.extend_from_slice(&(s.panes.len() as u16).to_le_bytes());
        for p in &s.panes {
            body.extend_from_slice(&p.sid.to_le_bytes());
            write_string(&mut body, &p.custom_title);
            write_string(&mut body, &p.last_cwd);
        }
        body.push(0);
        let parsed = read_from(&body).expect("parse");
        assert_eq!(parsed.grid_cols, 3);
        assert_eq!(parsed.grid_rows, 3);
        assert_eq!(parsed.focused_idx, 4);
        assert_eq!(parsed.panes.len(), 2);
        assert_eq!(parsed.panes[0].sid, 100);
        assert_eq!(parsed.panes[0].custom_title, "spg");
        assert_eq!(parsed.panes[1].sid, 0);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut body = Vec::new();
        body.extend_from_slice(&0xDEADBEEFu32.to_le_bytes());
        assert!(read_from(&body).is_none());
    }

    #[test]
    fn rejects_runaway_pane_count() {
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC.to_le_bytes());
        body.extend_from_slice(&VERSION.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes()); // cols
        body.extend_from_slice(&0u16.to_le_bytes()); // rows
        body.extend_from_slice(&0u16.to_le_bytes()); // focused
        body.extend_from_slice(&u16::MAX.to_le_bytes()); // 65535 panes
        assert!(read_from(&body).is_none());
    }
}
