//! F3+6 — marspot's L2 / L1 persistence layer.  Single binary file
//! at `~/Library/Caches/marspot/shell-state.bin` snapshots everything
//! a fresh marspot needs to restore the user's last layout.
//!
//! RFC-005 step 6 — the file is a **list of windows**, not one window
//! plus some fields.  Windows are peers: each carries its own grid,
//! focus and pane list, and none of them is "the" layout.  Before v2
//! the writer saved only the key window's panes, so opening a second
//! window (which immediately became key) overwrote a 16-pane record
//! with a 1-pane one and the next launch came up empty.
//!
//! Per window:
//!
//! - pane count + order (by index)
//! - per-pane (sid, custom_title, last_cwd) so the cold boot can
//!   reattach surviving L3s in the right slots, or fall back to
//!   spawning a fresh shell in the saved `last_cwd` when the L3
//!   didn't survive (kernel restart, manual `kill -9`, etc.)
//! - grid (cols × rows)
//! - focused pane index
//!
//! Window geometry lives in `window-state.bin` instead: L1 (AppKit)
//! owns frames, L2 owns panes, and one writer per file means no
//! atomic-rename race between them.  The two lists are both in window
//! creation order, which is what pairs entry *i* of one with entry
//! *i* of the other.
//!
//! Format is a plain LE binary header.  Atomic writes via `.tmp` +
//! rename.  Failure to parse / read returns `None` and the boot
//! falls through to the legacy registry-driven reattach.

use std::io::{self, Cursor, Read, Write};
use std::path::PathBuf;

const MAGIC: u32 = 0xA5505010;
const VERSION: u32 = 2;
/// Sanity ceiling — the modal caps grid at 6×6 = 36 + some headroom.
const MAX_PANES: usize = 128;
/// Sanity ceiling on the window list.  Well past any plausible
/// desktop; exists so a corrupt count can't drive a huge allocation.
const MAX_WINDOWS: usize = 64;
/// String length cap so a corrupt header can't trigger an OOM
/// allocation.  Real custom_title and cwd are bounded by PATH_MAX
/// (~1 KB) plus user-set labels typically < 64 chars.
const MAX_STR_BYTES: usize = 4096;

#[derive(Debug, Clone, Default)]
pub struct SavedState {
    /// Every open window, in creation order.  A one-window session is
    /// `len() == 1` — the degenerate case of the same shape, not a
    /// separate one.
    pub windows: Vec<SavedWindowLayout>,
    /// Index into `windows` of the window that had keyboard focus.
    /// Focus is the *only* thing that distinguishes a window here;
    /// restore uses it to decide which window comes up key.
    pub key_window: u16,
}

/// One window's layout: its grid shape, which of its panes had focus,
/// and the panes themselves in slot order.  (`SavedWindow`, further
/// down, is the geometry half — a different file, a different writer.)
#[derive(Debug, Clone, Default)]
pub struct SavedWindowLayout {
    pub grid_cols: u16,
    pub grid_rows: u16,
    pub focused_idx: u16,
    pub panes: Vec<SavedPane>,
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

/// F3+6.1 — separate file for the window frames so L1 (marspot-shell,
/// AppKit) can own writes without coordinating with L2 (marspot-core).
/// One file per writer = no atomic-rename race.
///
/// RFC-005 step 6 — v2 holds a *list* of frames in window creation
/// order, pairing index-for-index with `shell-state.bin`'s window
/// list.  v1 held exactly one, which is why any window could clobber
/// another window's geometry: whoever resized last won the file.
const WINDOW_MAGIC: u32 = 0xA5505011;
const WINDOW_VERSION: u32 = 2;

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

/// Best-effort read of every window's frame, in creation order.  Same
/// fail-soft policy as `read()` — returns None on missing / corrupt /
/// version drift.  A v1 file (single frame) reads as a one-element
/// list, which is exactly what it described.
pub fn read_windows() -> Option<Vec<SavedWindow>> {
    let body = std::fs::read(window_state_file_path()).ok()?;
    let mut cur = Cursor::new(body.as_slice());
    if read_u32(&mut cur)? != WINDOW_MAGIC { return None; }
    let count = match read_u32(&mut cur)? {
        1 => 1usize,
        2 => {
            let n = read_u16(&mut cur)? as usize;
            if n > MAX_WINDOWS { return None; }
            n
        }
        _ => return None,
    };
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let display_id = read_u32(&mut cur)?;
        let x = read_f64(&mut cur)?;
        let y = read_f64(&mut cur)?;
        let w = read_f64(&mut cur)?;
        let h = read_f64(&mut cur)?;
        out.push(SavedWindow { display_id, x, y, w, h });
    }
    Some(out)
}

/// Best-effort atomic write of every window's frame, in creation
/// order.  Always writes v2.
pub fn write_windows(frames: &[SavedWindow]) -> io::Result<()> {
    let path = window_state_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let n = frames.len().min(MAX_WINDOWS) as u16;
    let mut body: Vec<u8> = Vec::with_capacity(10 + 36 * n as usize);
    body.extend_from_slice(&WINDOW_MAGIC.to_le_bytes());
    body.extend_from_slice(&WINDOW_VERSION.to_le_bytes());
    body.extend_from_slice(&n.to_le_bytes());
    for s in frames.iter().take(n as usize) {
        body.extend_from_slice(&s.display_id.to_le_bytes());
        body.extend_from_slice(&s.x.to_le_bytes());
        body.extend_from_slice(&s.y.to_le_bytes());
        body.extend_from_slice(&s.w.to_le_bytes());
        body.extend_from_slice(&s.h.to_le_bytes());
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

// ─── Dev window persistence ───────────────────────────────────────
// Separate file from the main window's state so writes don't race
// against each other (each AppKit window writes its own state
// independently).  Same file-format style as `SavedWindow`.

/// Persisted frame for the UI-system dev panel's independent NSWindow.
/// Includes a `visible` bit so the user's "dev window open / closed"
/// preference also survives across launches.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SavedDevWindow {
    pub display_id: u32,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// True = open on next launch, false = hidden.
    pub visible: bool,
}

const DEV_WINDOW_MAGIC: u32 = 0xA5505012;
const DEV_WINDOW_VERSION: u32 = 1;

pub fn dev_window_state_file_path() -> PathBuf {
    let base: PathBuf = match std::env::var_os("MARSPOT_STATE_DIR") {
        Some(d) => PathBuf::from(d),
        None => {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join("Library/Caches/marspot")
        }
    };
    base.join("dev-window-state.bin")
}

pub fn read_dev_window() -> Option<SavedDevWindow> {
    let body = std::fs::read(dev_window_state_file_path()).ok()?;
    let mut cur = Cursor::new(body.as_slice());
    if read_u32(&mut cur)? != DEV_WINDOW_MAGIC { return None; }
    if read_u32(&mut cur)? != DEV_WINDOW_VERSION { return None; }
    let display_id = read_u32(&mut cur)?;
    let x = read_f64(&mut cur)?;
    let y = read_f64(&mut cur)?;
    let w = read_f64(&mut cur)?;
    let h = read_f64(&mut cur)?;
    let visible = read_u8(&mut cur)? != 0;
    Some(SavedDevWindow { display_id, x, y, w, h, visible })
}

pub fn write_dev_window(s: &SavedDevWindow) -> io::Result<()> {
    let path = dev_window_state_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut body: Vec<u8> = Vec::with_capacity(48);
    body.extend_from_slice(&DEV_WINDOW_MAGIC.to_le_bytes());
    body.extend_from_slice(&DEV_WINDOW_VERSION.to_le_bytes());
    body.extend_from_slice(&s.display_id.to_le_bytes());
    body.extend_from_slice(&s.x.to_le_bytes());
    body.extend_from_slice(&s.y.to_le_bytes());
    body.extend_from_slice(&s.w.to_le_bytes());
    body.extend_from_slice(&s.h.to_le_bytes());
    body.push(s.visible as u8);
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
    match read_u32(&mut cur)? {
        1 => read_v1_body(&mut cur),
        2 => read_v2_body(&mut cur),
        // Forward drift (a newer marspot wrote it, then the user
        // downgraded): fail soft to the registry-driven boot rather
        // than guess at a layout.
        _ => None,
    }
}

/// v1 — one window's worth of fields inline, then an unused window
/// frame that the writer never populated.  Read it as the single
/// window it describes; the next save rewrites the file as v2.
fn read_v1_body(cur: &mut Cursor<&[u8]>) -> Option<SavedState> {
    let grid_cols = read_u16(cur)?;
    let grid_rows = read_u16(cur)?;
    let focused_idx = read_u16(cur)?;
    let panes = read_panes(cur)?;
    Some(SavedState {
        windows: vec![SavedWindowLayout { grid_cols, grid_rows, focused_idx, panes }],
        key_window: 0,
    })
}

fn read_v2_body(cur: &mut Cursor<&[u8]>) -> Option<SavedState> {
    let key_window = read_u16(cur)?;
    let window_count = read_u16(cur)? as usize;
    if window_count > MAX_WINDOWS { return None; }
    let mut windows = Vec::with_capacity(window_count);
    for _ in 0..window_count {
        let grid_cols = read_u16(cur)?;
        let grid_rows = read_u16(cur)?;
        let focused_idx = read_u16(cur)?;
        let panes = read_panes(cur)?;
        windows.push(SavedWindowLayout { grid_cols, grid_rows, focused_idx, panes });
    }
    Some(SavedState { windows, key_window })
}

fn read_panes(cur: &mut Cursor<&[u8]>) -> Option<Vec<SavedPane>> {
    let pane_count = read_u16(cur)? as usize;
    if pane_count > MAX_PANES { return None; }
    let mut panes = Vec::with_capacity(pane_count);
    for _ in 0..pane_count {
        let sid = read_u64(cur)?;
        let custom_title = read_string(cur)?;
        let last_cwd = read_string(cur)?;
        panes.push(SavedPane { sid, custom_title, last_cwd });
    }
    Some(panes)
}

/// Atomic write — `.tmp` + rename.  Failures are logged; not
/// propagated.  Best-effort means a transient I/O hiccup doesn't
/// crash marspot.  Always writes v2.
pub fn write(s: &SavedState) -> io::Result<()> {
    let path = state_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut body: Vec<u8> = Vec::with_capacity(256);
    body.extend_from_slice(&MAGIC.to_le_bytes());
    body.extend_from_slice(&VERSION.to_le_bytes());
    body.extend_from_slice(&s.key_window.to_le_bytes());
    let n_windows = s.windows.len().min(MAX_WINDOWS) as u16;
    body.extend_from_slice(&n_windows.to_le_bytes());
    for w in s.windows.iter().take(n_windows as usize) {
        body.extend_from_slice(&w.grid_cols.to_le_bytes());
        body.extend_from_slice(&w.grid_rows.to_le_bytes());
        body.extend_from_slice(&w.focused_idx.to_le_bytes());
        let n = w.panes.len().min(MAX_PANES) as u16;
        body.extend_from_slice(&n.to_le_bytes());
        for p in w.panes.iter().take(n as usize) {
            body.extend_from_slice(&p.sid.to_le_bytes());
            write_string(&mut body, &p.custom_title);
            write_string(&mut body, &p.last_cwd);
        }
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

    /// `cargo test` runs tests as threads in one process and
    /// `MARSPOT_STATE_DIR` is process-global, so the two tests that
    /// touch real files serialise on this.  (nextest is process-per-
    /// test and immune, but keep `cargo test` correct too.)
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn pane(sid: u64, title: &str, cwd: &str) -> SavedPane {
        SavedPane { sid, custom_title: title.into(), last_cwd: cwd.into() }
    }

    /// Encode via the real writer's body builder by writing to a temp
    /// state dir, so the test can't drift from `write()`'s layout.
    fn round_trip(s: &SavedState) -> SavedState {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "marspot-state-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        // SAFETY: single-threaded within this test; the var is read
        // by `state_file_path` on this thread only.
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &dir) };
        write(s).expect("write");
        let out = read().expect("read back");
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    /// RFC-005 step 6 — the whole point of v2: every window survives,
    /// not just whichever one happened to be key at save time.
    #[test]
    fn round_trip_keeps_every_window() {
        let s = SavedState {
            key_window: 1,
            windows: vec![
                SavedWindowLayout {
                    grid_cols: 3, grid_rows: 3, focused_idx: 4,
                    panes: vec![
                        pane(100, "spg", "/Users/x/spg"),
                        pane(0, "", "/Users/x"),
                    ],
                },
                SavedWindowLayout {
                    grid_cols: 1, grid_rows: 2, focused_idx: 1,
                    panes: vec![pane(200, "notes", "/tmp")],
                },
            ],
        };
        let parsed = round_trip(&s);
        assert_eq!(parsed.key_window, 1);
        assert_eq!(parsed.windows.len(), 2);
        assert_eq!(parsed.windows[0].grid_cols, 3);
        assert_eq!(parsed.windows[0].focused_idx, 4);
        assert_eq!(parsed.windows[0].panes.len(), 2);
        assert_eq!(parsed.windows[0].panes[0].sid, 100);
        assert_eq!(parsed.windows[0].panes[0].custom_title, "spg");
        assert_eq!(parsed.windows[0].panes[1].sid, 0);
        assert_eq!(parsed.windows[1].grid_rows, 2);
        assert_eq!(parsed.windows[1].panes[0].sid, 200);
        assert_eq!(parsed.windows[1].panes[0].custom_title, "notes");
    }

    /// A v1 file written by any marspot up to 0.12.42 must still boot
    /// the user's layout — as the one window it always described.
    #[test]
    fn v1_file_reads_as_a_single_window() {
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes()); // VERSION 1
        body.extend_from_slice(&3u16.to_le_bytes()); // cols
        body.extend_from_slice(&3u16.to_le_bytes()); // rows
        body.extend_from_slice(&4u16.to_le_bytes()); // focused
        body.extend_from_slice(&2u16.to_le_bytes()); // pane count
        for p in [pane(100, "spg", "/Users/x/spg"), pane(0, "", "/Users/x")] {
            body.extend_from_slice(&p.sid.to_le_bytes());
            write_string(&mut body, &p.custom_title);
            write_string(&mut body, &p.last_cwd);
        }
        body.push(0); // v1's unused has_window byte
        let parsed = read_from(&body).expect("parse v1");
        assert_eq!(parsed.windows.len(), 1);
        assert_eq!(parsed.key_window, 0);
        assert_eq!(parsed.windows[0].grid_cols, 3);
        assert_eq!(parsed.windows[0].focused_idx, 4);
        assert_eq!(parsed.windows[0].panes.len(), 2);
        assert_eq!(parsed.windows[0].panes[0].custom_title, "spg");
    }

    #[test]
    fn rejects_bad_magic() {
        let mut body = Vec::new();
        body.extend_from_slice(&0xDEADBEEFu32.to_le_bytes());
        assert!(read_from(&body).is_none());
    }

    #[test]
    fn rejects_runaway_counts() {
        let head = |version: u32| {
            let mut b = Vec::new();
            b.extend_from_slice(&MAGIC.to_le_bytes());
            b.extend_from_slice(&version.to_le_bytes());
            b
        };
        let mut windows = head(2);
        windows.extend_from_slice(&0u16.to_le_bytes()); // key_window
        windows.extend_from_slice(&u16::MAX.to_le_bytes()); // 65535 windows
        assert!(read_from(&windows).is_none());

        let mut panes = head(2);
        panes.extend_from_slice(&0u16.to_le_bytes()); // key_window
        panes.extend_from_slice(&1u16.to_le_bytes()); // 1 window
        panes.extend_from_slice(&0u16.to_le_bytes()); // cols
        panes.extend_from_slice(&0u16.to_le_bytes()); // rows
        panes.extend_from_slice(&0u16.to_le_bytes()); // focused
        panes.extend_from_slice(&u16::MAX.to_le_bytes()); // 65535 panes
        assert!(read_from(&panes).is_none());

        // A version from the future is not guessed at.
        assert!(read_from(&head(99)).is_none());
    }

    /// Geometry file: same peer treatment, same v1 tolerance.
    #[test]
    fn window_frames_round_trip_as_a_list() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("marspot-frames-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        // SAFETY: single-threaded within this test.
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &dir) };
        let frames = vec![
            SavedWindow { display_id: 1, x: 0.0, y: 10.0, w: 800.0, h: 600.0 },
            SavedWindow { display_id: 2, x: 900.0, y: 20.0, w: 400.0, h: 300.0 },
        ];
        write_windows(&frames).expect("write");
        let back = read_windows().expect("read");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].w, 800.0);
        assert_eq!(back[1].display_id, 2);
        assert_eq!(back[1].x, 900.0);

        // v1: magic + version + one bare frame, no count.
        let mut v1 = Vec::new();
        v1.extend_from_slice(&WINDOW_MAGIC.to_le_bytes());
        v1.extend_from_slice(&1u32.to_le_bytes());
        v1.extend_from_slice(&7u32.to_le_bytes());
        for f in [1.0f64, 2.0, 3.0, 4.0] {
            v1.extend_from_slice(&f.to_le_bytes());
        }
        std::fs::write(window_state_file_path(), &v1).expect("write v1");
        let back = read_windows().expect("read v1");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].display_id, 7);
        assert_eq!(back[0].h, 4.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
