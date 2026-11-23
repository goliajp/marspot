//! Applying a snapshot: rebuilding the grid, cursor, modes and the
//! alt screen from a serialised blob.

use super::snapshot_clusters;
use super::snapshot_wire::{
    ALT_SECTION_FOLD, ALT_SECTION_LIVE, SNAPSHOT_MAGIC, SNAPSHOT_MIN_COMPAT, SNAPSHOT_VERSION,
    read_attrs, read_cell, read_u8, read_u16, read_u32, read_u64,
};
use super::{
    KittyKeyboardStack, MouseTrackingMode, Osc8, SavedCursor, SavedMain, ScreenModes, Terminal,
};
use crate::grid::{Cell, DEFAULT_SCROLLBACK_LINES, Grid};
use std::io::{self, Cursor};

impl Terminal {
    /// Inverse of `serialize_snapshot`.  Replaces the live grid +
    /// cursor + modes wholesale.  Any in-flight parser state,
    /// predictions, response queue, and cluster buffer are reset.
    /// Scrollback is NOT touched here — scrollback paging lives in
    /// step 8 via `GetScrollbackPage`.
    ///
    /// On format-version mismatch or any truncation: returns
    /// `InvalidData` and leaves the terminal untouched (we copy into
    /// the live state only after parsing succeeds).
    pub fn apply_snapshot(&mut self, body: &[u8]) -> io::Result<()> {
        let mut cur = Cursor::new(body);
        let magic = read_u32(&mut cur)?;
        if magic != SNAPSHOT_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("snapshot magic mismatch: 0x{:08x}", magic),
            ));
        }
        let snapshot_v = read_u32(&mut cur)?;
        if !(SNAPSHOT_MIN_COMPAT..=SNAPSHOT_VERSION).contains(&snapshot_v) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported snapshot version: {} (accept [{}, {}])",
                    snapshot_v, SNAPSHOT_MIN_COMPAT, SNAPSHOT_VERSION
                ),
            ));
        }
        let cols = read_u16(&mut cur)?;
        let rows = read_u16(&mut cur)?;
        let cursor_col = read_u16(&mut cur)?;
        let cursor_row = read_u16(&mut cur)?;
        let scroll_top = read_u16(&mut cur)?;
        let scroll_bot = read_u16(&mut cur)?;
        let modes = read_u32(&mut cur)?;
        let generation = read_u64(&mut cur)?;
        let attrs = read_attrs(&mut cur)?;
        let sc_present = read_u8(&mut cur)?;
        let saved_cursor = if sc_present == 1 {
            let col = read_u16(&mut cur)?;
            let row = read_u16(&mut cur)?;
            let sc_attrs = read_attrs(&mut cur)?;
            Some(SavedCursor {
                col,
                row,
                attrs: sc_attrs,
                // Deliberately not on the wire.  The deferred wrap of a
                // SAVED cursor is a transient bit that only matters
                // between a DECSC and its DECRC; carrying it would cost
                // a snapshot format bump, and every reader older than
                // that bump would have to be taught to skip it.  The
                // saved charset is not transient in the same way --
                // `?1049l` hands it back to the shell -- so it rides in
                // the mode bits, and is filled in below.
                pending_wrap: false,
                g0_graphics: (modes & (1 << 11)) != 0,
                origin_mode: (modes & (1 << 13)) != 0,
            })
        } else {
            None
        };
        let want_cells = cols as usize * rows as usize;
        let mut cells = Vec::with_capacity(want_cells);
        for _ in 0..want_cells {
            cells.push(read_cell(&mut cur)?);
        }
        // v2 trailing scrollback section: parsed *before* committing
        // anything to live state so a corrupt scrollback rejects the
        // whole apply, never half-loaded.  v1 stops here.
        //
        // F3+3.5 — sanity-cap `sb_count` + `line_cols` so a corrupt
        // u32 read (random bits or truncated file padded with
        // garbage) doesn't trigger a multi-GB `Vec::with_capacity`
        // → allocator abort.  Caps chosen well above any plausible
        // legitimate value (1M lines × 4096 cols = 16M cells max).
        //
        // v3 — header gains `start_logical_idx u64` before the count,
        // so the commit path can index-dedup against on-disk
        // scrollback (see F3+8 root-cause analysis,
        // `project_scrollback_execv_gap` memory).  v2 has no
        // start_idx and falls back to the F2+4 "skip replay if disk
        // has any content" heuristic.
        const MAX_SCROLLBACK_LINES_DESER: usize = 1_000_000;
        const MAX_LINE_COLS_DESER: usize = 4096;
        let mut snapshot_start_logical_idx: u64 = 0;
        let scrollback_lines: Vec<(Vec<Cell>, bool)> = if snapshot_v >= 2 {
            if snapshot_v >= 3 {
                snapshot_start_logical_idx = read_u64(&mut cur)?;
            }
            let sb_count = read_u32(&mut cur)? as usize;
            if sb_count > MAX_SCROLLBACK_LINES_DESER {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "snapshot scrollback line count {} exceeds {} cap",
                        sb_count, MAX_SCROLLBACK_LINES_DESER
                    ),
                ));
            }
            let mut out = Vec::with_capacity(sb_count);
            for _ in 0..sb_count {
                let wrapped = read_u8(&mut cur)? != 0;
                let line_cols = read_u32(&mut cur)? as usize;
                if line_cols > MAX_LINE_COLS_DESER {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "snapshot line width {} exceeds {} cap",
                            line_cols, MAX_LINE_COLS_DESER
                        ),
                    ));
                }
                let mut line = Vec::with_capacity(line_cols);
                for _ in 0..line_cols {
                    line.push(read_cell(&mut cur)?);
                }
                out.push((line, wrapped));
            }
            out
        } else {
            Vec::new()
        };
        // v4/v5 alt section — parsed before commit like everything
        // else.  Absent (or v ≤ 3) → empty.  kind: 0 none, 1 fold
        // (death snapshot → lines land in scrollback), 2 live (execv
        // → rebuild the alt screen verbatim; see AltLive below).
        struct AltLive {
            cols: u16,
            rows: u16,
            cursor: (u16, u16),
            scroll_top: u16,
            scroll_bot: u16,
            ring: Vec<(Vec<Cell>, bool)>,
            cells: Vec<Cell>,
        }
        let mut alt_live: Option<AltLive> = None;
        let alt_fold_lines: Vec<(Vec<Cell>, bool)> = if snapshot_v >= 4 {
            let present = read_u8(&mut cur)?;
            if present == ALT_SECTION_LIVE {
                let a_cols = read_u16(&mut cur)?;
                let a_rows = read_u16(&mut cur)?;
                let acc = read_u16(&mut cur)?;
                let acr = read_u16(&mut cur)?;
                let a_st = read_u16(&mut cur)?;
                let a_sb = read_u16(&mut cur)?;
                let ring_n = read_u32(&mut cur)? as usize;
                if ring_n > MAX_SCROLLBACK_LINES_DESER || (a_cols as usize) > MAX_LINE_COLS_DESER {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "snapshot alt-live section exceeds caps",
                    ));
                }
                let mut ring = Vec::with_capacity(ring_n);
                for _ in 0..ring_n {
                    let wrapped = read_u8(&mut cur)? != 0;
                    let line_cols = read_u32(&mut cur)? as usize;
                    if line_cols > MAX_LINE_COLS_DESER {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "snapshot alt-live line width exceeds cap",
                        ));
                    }
                    let mut line = Vec::with_capacity(line_cols);
                    for _ in 0..line_cols {
                        line.push(read_cell(&mut cur)?);
                    }
                    ring.push((line, wrapped));
                }
                let want = a_cols as usize * a_rows as usize;
                let mut cells = Vec::with_capacity(want);
                for _ in 0..want {
                    cells.push(read_cell(&mut cur)?);
                }
                alt_live = Some(AltLive {
                    cols: a_cols,
                    rows: a_rows,
                    cursor: (acc, acr),
                    scroll_top: a_st,
                    scroll_bot: a_sb,
                    ring,
                    cells,
                });
                Vec::new()
            } else if present == ALT_SECTION_FOLD {
                let count = read_u32(&mut cur)? as usize;
                if count > MAX_SCROLLBACK_LINES_DESER {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "snapshot alt-fold line count {} exceeds {} cap",
                            count, MAX_SCROLLBACK_LINES_DESER
                        ),
                    ));
                }
                let mut out = Vec::with_capacity(count);
                for _ in 0..count {
                    let wrapped = read_u8(&mut cur)? != 0;
                    let line_cols = read_u32(&mut cur)? as usize;
                    if line_cols > MAX_LINE_COLS_DESER {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "snapshot alt-fold line width {} exceeds {} cap",
                                line_cols, MAX_LINE_COLS_DESER
                            ),
                        ));
                    }
                    let mut line = Vec::with_capacity(line_cols);
                    for _ in 0..line_cols {
                        line.push(read_cell(&mut cur)?);
                    }
                    out.push((line, wrapped));
                }
                out
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        let trailer = snapshot_clusters::Trailer::read(&mut cur);
        // All parsing OK — commit to live state.
        self.grid.resize(cols, rows);
        for r in 0..rows {
            for c in 0..cols {
                let idx = r as usize * cols as usize + c as usize;
                self.grid.set_cell(c, r, cells[idx]);
            }
        }
        self.grid.set_cursor(cursor_col, cursor_row);
        self.scroll_top = scroll_top;
        self.scroll_bot = scroll_bot;
        self.cursor_key_application_mode = (modes & (1 << 0)) != 0;
        self.bracketed_paste_mode.on = (modes & (1 << 1)) != 0;
        self.cursor_visible = (modes & (1 << 2)) != 0;
        self.pending_wrap = (modes & (1 << 3)) != 0;
        // bits 5-6 = mouse_tracking_mode,bit 7 = SGR(0.6.66 起加).
        // 老 snapshot 不会有这些 bit → 自然 Off,跟老语义一致.
        self.mouse_tracking_mode = match (modes >> 5) & 0b11 {
            1 => MouseTrackingMode::X11,
            2 => MouseTrackingMode::ButtonEvent,
            3 => MouseTrackingMode::AnyEvent,
            _ => MouseTrackingMode::Off,
        };
        self.mouse_sgr_encoding = (modes & (1 << 7)) != 0;
        self.alt_scroll = (modes & (1 << 8)) != 0;
        self.focus_reporting = (modes & (1 << 9)) != 0;
        self.g0_graphics = (modes & (1 << 10)) != 0;
        self.origin_mode = (modes & (1 << 12)) != 0;
        self.insert_mode = (modes & (1 << 14)) != 0;
        self.screen_modes = ScreenModes::from_bits(modes, std::mem::take(&mut self.screen_modes.palette));
        // Bit 4 (in_alt_screen) is informational for the wire format
        // but not actionable here — apply_snapshot replaces the
        // current grid; alt-mode save state is regenerated on the
        // next `?1049h` toggle from the PTY stream.  The EXECV path
        // does not rely on it: `serialize_snapshot_live` carries the
        // alt screen verbatim and rebuilds `saved_main` underneath.
        self.attrs = attrs;
        self.osc8 = Osc8::default();
        self.saved_cursor = saved_cursor;
        self.generation = generation;
        // Reset transient state so a half-feed cluster / prediction
        // queue / response buffer doesn't bleed across the snapshot.
        self.predictions.clear();
        self.shadow.clear();
        self.cluster_buf.clear();
        self.cluster_fast = None;
        self.grapheme_cursor = crate::grapheme::GraphemeCursor::new();
        self.seg_synced = true;
        self.pending_response.clear();
        self.replay_history(
            snapshot_v,
            snapshot_start_logical_idx,
            scrollback_lines,
            alt_fold_lines,
            &trailer,
        );
        // v5 alt LIVE — rebuild the alt screen exactly as it was at
        // execv time: main grid (just restored above) moves into
        // saved_main, a fresh Memory-backed alt grid takes over with
        // the serialized ring + cells + cursor + scroll region.  The
        // still-running TUI's incremental repaints then land on the
        // same base they left.
        let kept_alt = alt_live.is_some();
        if let Some(live) = alt_live {
            let mut alt = Grid::with_scrollback(live.cols, live.rows, DEFAULT_SCROLLBACK_LINES);
            trailer.push_alt_ring(&mut alt, &live.ring);
            for r in 0..live.rows {
                for c in 0..live.cols {
                    let idx = r as usize * live.cols as usize + c as usize;
                    alt.set_cell(c, r, live.cells[idx]);
                }
            }
            alt.set_cursor(live.cursor.0, live.cursor.1);
            let main = std::mem::replace(&mut self.grid, alt);
            self.saved_main = Some(SavedMain {
                grid: main,
                cursor: (cursor_col, cursor_row),
                // A snapshot carries screen contents, not modes.  The
                // kitty stack rebuilds itself the same way DECCKM and
                // bracketed paste do: from the program's own bytes as
                // the bytelog replays.
                kitty_keyboard: KittyKeyboardStack::default(),
            });
            self.scroll_top = live.scroll_top;
            self.scroll_bot = live.scroll_bot;
        }
        self.apply_snapshot_clusters(&trailer, kept_alt);
        Ok(())
    }
}
