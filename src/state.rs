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
const VERSION: u32 = 3;
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

/// `SavedPane.flags` bit 0 — RFC-006 dormant placeholder: the slot a
/// moved-out pane left behind.  Restored as a placeholder directly;
/// never reattached, never resurrected, never spawned.
pub const PANE_FLAG_DORMANT: u8 = 1;

#[derive(Debug, Clone, Default)]
pub struct SavedPane {
    /// Shelld session id, or 0 for "no surviving sid" (in-process /
    /// pane saved before a sid was assigned).
    pub sid: u64,
    /// v3 — see `PANE_FLAG_DORMANT`.  v1/v2 files read as 0.
    pub flags: u8,
    /// User-set custom title.  Empty = no custom title (cwd basename
    /// or ordinal applies at render time).
    /// Dead field, kept so the on-disk format does not change: an
    /// older build must still be able to read what this one writes,
    /// and vice versa.  Panes are not named by anyone any more — a
    /// pane's name is derived from its directory
    /// (`marspot::pane_name`), so nothing sets this and nothing reads
    /// it back into a pane.
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
    marspot_term::paths::state_root().join("window-state.bin")
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

/// Forget every saved window frame.
///
/// Called on the one teardown that means "there is nothing to come
/// back to": the user closed the last pane of the last window.  The
/// next launch then has no geometry to restore and opens the default
/// window centred, rather than reusing the frame of a layout the user
/// just dismantled.
pub fn clear_windows() -> io::Result<()> {
    refuse_if_test_binary("window-state.bin")?;
    match std::fs::remove_file(window_state_file_path()) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// Refuse to write the real, installed app's state from a test.
///
/// `MARSPOT_STATE_DIR` unset means "the user's live layout", and a
/// test that reaches any save path — directly, or through a `CoreApp`
/// method that happens to persist — overwrites it with a fixture.  On
/// 2026-07-27 that turned a 4×4 / 16-pane record into two 1×1 windows
/// and the next core boot came up with one window and every session
/// re-adopted as an orphan.  Sessions survived; the layout did not.
///
/// Any one of three things makes a process a test: this crate was
/// compiled for its own tests, nextest started the process, or the
/// executable is one cargo built for a test target.  An `Err` here is
/// loud where the write was silent.
fn refuse_if_test_binary(what: &str) -> io::Result<()> {
    if std::env::var_os("MARSPOT_STATE_DIR").is_some() {
        return Ok(());
    }
    let exe = std::env::current_exe().unwrap_or_default();
    if cfg!(test) || under_nextest() || is_cargo_test_executable(&exe) {
        return Err(io::Error::other(format!(
            "refusing to write the installed app's {what} from a test \
             binary ({}); set MARSPOT_STATE_DIR to a sandbox",
            exe.display()
        )));
    }
    Ok(())
}

/// nextest started this process, or one above it: it sets `NEXTEST`
/// for every test it runs, and what a test starts inherits it.
///
/// `CARGO_MANIFEST_DIR` is not asked: `cargo run` sets it as well, so
/// it does not tell a test from a build somebody is trying out.
fn under_nextest() -> bool {
    std::env::var_os("NEXTEST").is_some()
}

/// `exe` is named or kept the way cargo does it for a test target: a
/// file called `<crate>-<16 hex digits>`, or anything in a `deps/`
/// directory.  Nothing shipped is either.
///
/// This is what speaks for a test binary that nothing else announces —
/// another target's, run by `cargo test` or by hand.  Cargo promises
/// neither the name nor the place: `deps/` used to be the whole check,
/// cargo 1.100 moved test executables to `build/<package>/<hash>/out/`,
/// and from that day the guard refused nothing.
fn is_cargo_test_executable(exe: &std::path::Path) -> bool {
    let in_deps = exe
        .parent()
        .is_some_and(|d| d.file_name().is_some_and(|n| n == "deps"));
    let hashed = exe
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.rsplit_once('-'))
        .is_some_and(|(_, hash)| {
            hash.len() == 16 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        });
    in_deps || hashed
}

/// Best-effort atomic write of every window's frame, in creation
/// order.  Always writes v2.
pub fn write_windows(frames: &[SavedWindow]) -> io::Result<()> {
    refuse_if_test_binary("window-state.bin")?;
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

/// Resolve `~/Library/Caches/marspot/shell-state.bin`, respecting
/// `MARSPOT_STATE_DIR` for dev sandbox / test paths.
pub fn state_file_path() -> PathBuf {
    marspot_term::paths::state_root().join("shell-state.bin")
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
        2 => read_v2_body(&mut cur, 2),
        3 => read_v2_body(&mut cur, 3),
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
    let panes = read_panes(cur, 1)?;
    Some(SavedState {
        windows: vec![SavedWindowLayout { grid_cols, grid_rows, focused_idx, panes }],
        key_window: 0,
    })
}

fn read_v2_body(cur: &mut Cursor<&[u8]>, version: u32) -> Option<SavedState> {
    let key_window = read_u16(cur)?;
    let window_count = read_u16(cur)? as usize;
    if window_count > MAX_WINDOWS { return None; }
    let mut windows = Vec::with_capacity(window_count);
    for _ in 0..window_count {
        let grid_cols = read_u16(cur)?;
        let grid_rows = read_u16(cur)?;
        let focused_idx = read_u16(cur)?;
        let panes = read_panes(cur, version)?;
        windows.push(SavedWindowLayout { grid_cols, grid_rows, focused_idx, panes });
    }
    Some(SavedState { windows, key_window })
}

fn read_panes(cur: &mut Cursor<&[u8]>, version: u32) -> Option<Vec<SavedPane>> {
    let pane_count = read_u16(cur)? as usize;
    if pane_count > MAX_PANES { return None; }
    let mut panes = Vec::with_capacity(pane_count);
    for _ in 0..pane_count {
        let sid = read_u64(cur)?;
        // v3 inserts flags between sid and the strings; older files
        // simply don't have it.
        let flags = if version >= 3 { read_u8(cur)? } else { 0 };
        let custom_title = read_string(cur)?;
        let last_cwd = read_string(cur)?;
        panes.push(SavedPane { sid, flags, custom_title, last_cwd });
    }
    Some(panes)
}

/// Atomic write — `.tmp` + rename.  Failures are logged; not
/// propagated.  Best-effort means a transient I/O hiccup doesn't
/// crash marspot.  Always writes v2.
pub fn write(s: &SavedState) -> io::Result<()> {
    refuse_if_test_binary("shell-state.bin")?;
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
            body.push(p.flags);
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

/// Forget the saved layout — the pane/window half of `clear_windows`.
///
/// Removing the file rather than writing an empty record matters: an
/// empty *list* and a missing file both boot to defaults, but only the
/// missing file survives a downgrade to a build that reads v1.
pub fn clear() -> io::Result<()> {
    refuse_if_test_binary("shell-state.bin")?;
    match std::fs::remove_file(state_file_path()) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
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

    fn pane(sid: u64, title: &str, cwd: &str) -> SavedPane {
        SavedPane { sid, flags: 0, custom_title: title.into(), last_cwd: cwd.into() }
    }

    /// Encode via the real writer's body builder by writing to a temp
    /// state dir, so the test can't drift from `write()`'s layout.
    fn round_trip(s: &SavedState) -> SavedState {
        let _sandbox = crate::test_env::sandbox_state_dir();
        write(s).expect("write");
        read().expect("read back")
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

    /// RFC-006 — the dormant flag survives the round trip; a v2 file
    /// (no flags byte) reads as all-zero flags.
    #[test]
    fn dormant_flag_round_trips_and_v2_reads_as_zero() {
        let s = SavedState {
            key_window: 0,
            windows: vec![SavedWindowLayout {
                grid_cols: 2, grid_rows: 1, focused_idx: 0,
                panes: vec![
                    pane(100, "live", "/x"),
                    SavedPane {
                        sid: 0,
                        flags: PANE_FLAG_DORMANT,
                        custom_title: String::new(),
                        last_cwd: String::new(),
                    },
                ],
            }],
        };
        let parsed = round_trip(&s);
        assert_eq!(parsed.windows[0].panes[0].flags, 0);
        assert_eq!(parsed.windows[0].panes[1].flags, PANE_FLAG_DORMANT);

        // Hand-built v2 body: sid + strings, no flags byte.
        let mut body = Vec::new();
        body.extend_from_slice(&MAGIC.to_le_bytes());
        body.extend_from_slice(&2u32.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes()); // key
        body.extend_from_slice(&1u16.to_le_bytes()); // 1 window
        body.extend_from_slice(&1u16.to_le_bytes()); // cols
        body.extend_from_slice(&1u16.to_le_bytes()); // rows
        body.extend_from_slice(&0u16.to_le_bytes()); // focused
        body.extend_from_slice(&1u16.to_le_bytes()); // 1 pane
        body.extend_from_slice(&7u64.to_le_bytes());
        write_string(&mut body, "t");
        write_string(&mut body, "/c");
        let parsed = read_from(&body).expect("v2 parses");
        assert_eq!(parsed.windows[0].panes[0].sid, 7);
        assert_eq!(parsed.windows[0].panes[0].flags, 0, "v2 = no flags = 0");
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

    /// The net that would have saved the 2026-07-27 layout: with no
    /// sandbox set, a write from a test binary must be refused rather
    /// than land on the installed app's file.
    ///
    /// `HOME` is a directory of the test's own, so a guard that fails
    /// to refuse writes there: this test has failed by overwriting the
    /// very record it is here to protect.
    #[test]
    fn a_test_binary_cannot_write_the_installed_apps_state() {
        let home = crate::test_env::TempDir::new("state-home");
        let mut env = crate::test_env::EnvGuard::new();
        env.remove("MARSPOT_STATE_DIR").set("HOME", home.path());

        let err = write(&SavedState::default()).expect_err("write must be refused");
        assert!(
            err.to_string().contains("refusing to write"),
            "unexpected error: {err}"
        );
        assert!(write_windows(&[]).is_err());
        assert!(clear().is_err());
        assert!(clear_windows().is_err());
        assert!(!home.join("Library").exists(), "a refused write leaves nothing behind");
    }

    #[test]
    fn what_nextest_starts_is_a_test() {
        let mut env = crate::test_env::EnvGuard::new();
        env.remove("NEXTEST");
        assert!(!under_nextest());
        env.set("NEXTEST", "1");
        assert!(under_nextest());
    }

    /// The honesty check on the part of the guard that reads a path:
    /// this very executable has to be recognised, so the day cargo
    /// names or places test executables some other way this fails and
    /// says so, instead of leaving a check that quietly matches nothing.
    #[test]
    fn a_test_executable_is_known_by_its_name_or_its_place() {
        use std::path::Path;
        let me = std::env::current_exe().expect("this test's executable");
        assert!(is_cargo_test_executable(&me), "{} is not taken for a test executable", me.display());

        for test in [
            // through cargo 1.99
            "/w/target/debug/deps/marspot-83909a48e552b970",
            "/w/target/release/deps/marspot_core-8f7d7eeca4da8079",
            "/w/target/debug/deps/it",
            // from cargo 1.100
            "/w/target/debug/build/marspot/10cc83810a0598dc/out/marspot-10cc83810a0598dc",
            "/w/target/debug/build/marspot/dfaf590f12403b74/out/marspot_core-dfaf590f12403b74",
            "/copied/elsewhere/marspot_shell-56bc4bd482e84660",
        ] {
            assert!(is_cargo_test_executable(Path::new(test)), "{test}");
        }
        for shipped in [
            "/Users/u/.local/Marspot.app/Contents/MacOS/marspot-shell",
            "/Applications/Marspot.app/Contents/MacOS/marspot-core",
            "/Users/u/Library/Application Support/marspot/binaries/current/marspot-core",
            "/Users/u/Library/Application Support/marspot/binaries/pending/marspot-shell",
            "/w/target/release/marspot-core",
            "/w/target/debug/build/marspot/0100c0c45907a006/out/marspot_core",
            "/w/target/debug/mcli",
            "",
        ] {
            assert!(!is_cargo_test_executable(Path::new(shipped)), "{shipped}");
        }
    }

    /// Geometry file: same peer treatment, same v1 tolerance.
    #[test]
    fn window_frames_round_trip_as_a_list() {
        let _sandbox = crate::test_env::sandbox_state_dir();
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
    }
}
