//! Snapshot wire format helpers: the version constants and the cell,
//! attribute and integer encodings that snapshots and scrollback pages
//! share.

use crate::grid::{Cell, CellAttrs, Color, ColorKind};
use std::io::{self, Cursor};

pub(super) const SNAPSHOT_MAGIC: u32 = 0xA557_5301;
/// v1: live grid + cursor + modes only (scrollback was lost across
///     L3 self-execv silent updates — user reported as "大部分窗口
///     没几行历史").
/// v2: trailing scrollback section appended.  Reader honours
///     `[SNAPSHOT_MIN_COMPAT, SNAPSHOT_VERSION]` so a v1-payload
///     dropped by an older L3 still applies after a forward update,
///     and a v1-image reader of a v2-payload just ignores the
///     trailing scrollback (silent + lossless wire upgrade — see
///     `feedback_wire_upgrade_silent_lossless` in handoff memory).
/// v3: scrollback section is self-describing.  Header now carries
///     `start_logical_idx: u64` BEFORE the line count, naming the
///     logical line index of the FIRST scrollback line in the body.
///     `apply_snapshot` dedups by index: it replays only lines whose
///     logical idx ≥ on-disk scrollback length, so any gap between
///     persistent (file) scrollback and snapshot (RAM ring) gets
///     filled automatically.  Pre-v3 the receiver either replayed
///     everything (duplicating disk's tail) or skipped entirely (the
///     F2+4 optimisation), and the latter silently lost any rows the
///     writer hadn't yet flushed at the moment of L3 self-execv —
///     diagnosed 2026-06-21 as "scrollback rows被吞" (sid 281
///     insight pane, long-running tables).  v3 makes the dedup
///     correct by construction; F2+4 stays for v2 fall-back only.
// v4 (RFC-004 C.1): main layer always serializes the REAL grid (in
// alt mode that's `saved_main`, whose scrollback index space matches
// the on-disk File — v3 got this wrong and dropped alt history on
// apply), plus a trailing alt-fold section appending the alt ring +
// final alt screen into scrollback on apply.  v3 payloads (no alt
// section) still apply — the section is read only when v >= 4.
//
// v5 (RFC-004 C.1 amendment, 2026-07-17 field regression): the alt
// section gains a KIND byte — 0 = none, 1 = fold, 2 = LIVE.  Fold is
// for death snapshots (periodic / clean SIGTERM): the process is
// gone, a fresh shell follows, so the alt content's only future is
// as scrollback history.  LIVE is for execv handoff: the TUI behind
// the PTY is still running and still believes it owns an alt screen
// — folding here swapped the visible grid to the pre-TUI main
// screen, and the TUI's incremental repaints landed on the wrong
// base (field report: claudecode's input box vanished until its
// next full repaint).  LIVE serializes the alt screen VERBATIM
// (dims + cursor + scroll region + ring + cells); apply rebuilds
// `saved_main` + the alt grid exactly, so post-execv the terminal
// state is bit-identical and incremental repaints stay seamless.
pub(super) const SNAPSHOT_VERSION: u32 = 5;
pub(super) const SNAPSHOT_MIN_COMPAT: u32 = 1;
pub(super) const ALT_SECTION_NONE: u8 = 0;
pub(super) const ALT_SECTION_FOLD: u8 = 1;
pub(super) const ALT_SECTION_LIVE: u8 = 2;
/// Cap on how many of the most-recent scrollback lines we serialise
/// across an execv.  Sized so an 8-pane window full of long
/// claudecode sessions still finishes its state.bin IO in well under
/// a second on NVMe.
///
/// Phase-2 plan (see project memory `project-scrollback-persistent`):
/// move scrollback to a per-session file (`scrollback.bin`) that
/// L3 reopens after execv instead of replaying through snapshot.
/// Once that lands, this cap goes away and depth becomes unbounded
/// (with a lazy-load window for daily perf).  Until then 20 000
/// lines × 80 cols × ~16 B/cell ≈ 26 MB per pane covers typical
/// usage; 8 panes = ~200 MB writes during install-local, which is
/// roughly 200 ms on the dev mini.  Acceptable for an event the
/// user triggers manually.
pub(super) const SNAPSHOT_SCROLLBACK_LINE_CAP: usize = 20_000;
pub(super) const ATTRS_BYTES: usize = 9;
pub(super) const CELL_BYTES: usize = 4 + ATTRS_BYTES;


pub fn serialize_attrs_pub(a: CellAttrs) -> [u8; ATTRS_BYTES] {
    serialize_attrs(a)
}

pub fn deserialize_attrs_pub(buf: &[u8]) -> CellAttrs {
    debug_assert!(
        buf.len() >= ATTRS_BYTES,
        "attrs slice shorter than ATTRS_BYTES"
    );
    let flags = buf[0];
    let fg_kind = buf[1];
    let fg_payload = [buf[2], buf[3], buf[4]];
    let bg_kind = buf[5];
    let bg_payload = [buf[6], buf[7], buf[8]];
    CellAttrs {
        bold: (flags & (1 << 0)) != 0,
        italic: (flags & (1 << 1)) != 0,
        underline: (flags & (1 << 2)) != 0,
        reverse: (flags & (1 << 3)) != 0,
        dim: (flags & (1 << 4)) != 0,
        fg: decode_color(fg_kind, fg_payload).unwrap_or(Color::DEFAULT),
        bg: decode_color(bg_kind, bg_payload).unwrap_or(Color::DEFAULT),
        _pad: [unshown_flags(flags), 0, 0],
    }
}

/// SGR 8 and 5 from bits 5-6 of a serialised attribute set.
fn unshown_flags(flags: u8) -> u8 {
    ((flags >> 5 & 1) * crate::grid::FLAG_INVISIBLE) | ((flags >> 6 & 1) * crate::grid::FLAG_BLINK)
}

pub(super) fn serialize_attrs(a: CellAttrs) -> [u8; ATTRS_BYTES] {
    let mut flags = 0u8;
    if a.bold {
        flags |= 1 << 0;
    }
    if a.italic {
        flags |= 1 << 1;
    }
    if a.underline {
        flags |= 1 << 2;
    }
    if a.reverse {
        flags |= 1 << 3;
    }
    if a.dim {
        flags |= 1 << 4;
    }
    if a.invisible() {
        flags |= 1 << 5;
    }
    if a.blink() {
        flags |= 1 << 6;
    }
    let (fg_kind, fg_payload) = encode_color(a.fg);
    let (bg_kind, bg_payload) = encode_color(a.bg);
    let mut out = [0u8; ATTRS_BYTES];
    out[0] = flags;
    out[1] = fg_kind;
    out[2..5].copy_from_slice(&fg_payload);
    out[5] = bg_kind;
    out[6..9].copy_from_slice(&bg_payload);
    out
}

fn encode_color(c: Color) -> (u8, [u8; 3]) {
    match c.kind() {
        ColorKind::Default => (0, [0, 0, 0]),
        ColorKind::Indexed(i) => (1, [i, 0, 0]),
        ColorKind::Rgb(r, g, b) => (2, [r, g, b]),
    }
}

fn decode_color(kind: u8, payload: [u8; 3]) -> io::Result<Color> {
    Ok(match kind {
        0 => Color::DEFAULT,
        1 => Color::indexed(payload[0]),
        2 => Color::rgb(payload[0], payload[1], payload[2]),
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown color kind {}", other),
            ));
        }
    })
}

/// One snapshot cell: its character, then its attributes.
pub(super) fn read_cell(cur: &mut Cursor<&[u8]>) -> io::Result<Cell> {
    let ch = char::from_u32(read_u32(cur)?).unwrap_or(' ');
    Ok(Cell { ch, attrs: read_attrs(cur)? })
}

pub(super) fn read_attrs(cur: &mut Cursor<&[u8]>) -> io::Result<CellAttrs> {
    let mut buf = [0u8; ATTRS_BYTES];
    use std::io::Read;
    cur.read_exact(&mut buf)?;
    let flags = buf[0];
    let fg_kind = buf[1];
    let fg_payload = [buf[2], buf[3], buf[4]];
    let bg_kind = buf[5];
    let bg_payload = [buf[6], buf[7], buf[8]];
    Ok(CellAttrs {
        bold: (flags & (1 << 0)) != 0,
        italic: (flags & (1 << 1)) != 0,
        underline: (flags & (1 << 2)) != 0,
        reverse: (flags & (1 << 3)) != 0,
        dim: (flags & (1 << 4)) != 0,
        fg: decode_color(fg_kind, fg_payload)?,
        bg: decode_color(bg_kind, bg_payload)?,
        _pad: [unshown_flags(flags), 0, 0],
    })
}

pub(super) fn read_u8(cur: &mut Cursor<&[u8]>) -> io::Result<u8> {
    let mut b = [0u8; 1];
    use std::io::Read;
    cur.read_exact(&mut b)?;
    Ok(b[0])
}

pub(super) fn read_u16(cur: &mut Cursor<&[u8]>) -> io::Result<u16> {
    let mut b = [0u8; 2];
    use std::io::Read;
    cur.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

pub(super) fn read_u32(cur: &mut Cursor<&[u8]>) -> io::Result<u32> {
    let mut b = [0u8; 4];
    use std::io::Read;
    cur.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

pub(super) fn read_u64(cur: &mut Cursor<&[u8]>) -> io::Result<u64> {
    let mut b = [0u8; 8];
    use std::io::Read;
    cur.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
