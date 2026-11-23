//! Serialising a snapshot: the whole terminal state as one
//! self-contained blob, with or without the alt screen kept live.

use super::snapshot_clusters;
use super::snapshot_wire::{
    ALT_SECTION_FOLD, ALT_SECTION_LIVE, ALT_SECTION_NONE, CELL_BYTES, SNAPSHOT_MAGIC,
    SNAPSHOT_SCROLLBACK_LINE_CAP, SNAPSHOT_VERSION, serialize_attrs,
};
use super::{MouseTrackingMode, Terminal};
use crate::grid::Cell;

impl Terminal {
    /// Serialize the full terminal state into a self-contained byte
    /// blob.  RFC-002 §4.3: the snapshot pump (L3) ships this to
    /// shelld in a `SaveSnapshot` frame; ATTACH (L4) ships it to the
    /// client in `StateSnapshot`; the client's `apply_snapshot`
    /// (inverse below) jumps a fresh `Terminal` straight to this
    /// state without re-parsing any PTY bytes.
    ///
    /// Wire format (internal, versioned independently from
    /// `shelld_proto::PROTO_VERSION`):
    ///
    /// ```text
    /// [magic        u32 LE = 0xA557_5301]
    /// [snapshot_v   u32 LE = 1]
    /// [cols, rows   u16 LE × 2]
    /// [cursor       u16 LE × 2  — col, row]
    /// [scroll_top/bot u16 LE × 2]
    /// [mode_flags   u32 LE  — bit 0 cursor_key_app, 1 bracketed_paste,
    ///                          2 cursor_visible,    3 pending_wrap,
    ///                          4 in_alt_screen]
    /// [generation   u64 LE]
    /// [attrs        9 bytes — current SGR state]
    /// [saved_cursor_present u8]
    /// [saved_cursor body if present: col u16 + row u16 + attrs 9]
    /// [cells: rows × cols × 13 bytes, row-major]
    ///   each cell:
    ///     [ch  u32 LE]
    ///     [flags u8]  - bold/italic/underline/reverse/dim packed bits
    ///     [fg_kind u8][fg_payload 3 bytes]
    ///     [bg_kind u8][bg_payload 3 bytes]
    /// ```
    ///
    /// Cell width: 13 bytes/cell.  A 97×75 grid serializes to ~95 KB.
    ///
    /// RFC-004 C.1 (snapshot v4) — alt-screen FOLD.  When the
    /// terminal is inside `?1049h` (claudecode / vim / any TUI), the
    /// MAIN layer (the grid + File scrollback hiding in `saved_main`)
    /// is what serializes as the snapshot body — its
    /// `start_logical_idx` is in the File's index space, which is
    /// what apply's dedup skip arithmetic assumes.  (v3 serialized
    /// whatever grid was current; in alt mode that was the alt grid
    /// with its RAM-ring index space, and apply's skip computed
    /// against the File count silently dropped alt history — B13.)
    /// The alt layer (its ring history + final visible rows) is
    /// appended as a v4 trailing section; apply folds those lines
    /// into scrollback, so a resurrected claudecode pane keeps its
    /// whole conversation as scrollable history above a fresh shell.
    /// Restoring INTO alt mode stays deliberately unsupported — the
    /// user expects to re-launch the TUI, not have it half-resumed.
    pub fn serialize_snapshot(&self) -> Vec<u8> {
        self.serialize_snapshot_impl(SNAPSHOT_SCROLLBACK_LINE_CAP, false)
    }

    /// v5 LIVE snapshot for the execv handoff: the process (and the
    /// TUI on its PTY) survives the swap, so an alt screen must come
    /// back VERBATIM — dims, cursor, scroll region, ring, cells —
    /// with `saved_main` rebuilt underneath.  Fold semantics here
    /// broke incremental repaints (see the v5 const comment).
    /// Non-alt terminals serialize identically to the fold form.
    pub fn serialize_snapshot_live(&self) -> Vec<u8> {
        self.serialize_snapshot_impl(SNAPSHOT_SCROLLBACK_LINE_CAP, true)
    }

    /// `serialize_snapshot` with a caller-chosen cap on the MAIN
    /// scrollback tail section.  The periodic-snapshot path (RFC-004
    /// C.1) flushes the File scrollback first, so its tail only needs
    /// to cover a residual sliver — passing a small cap keeps the
    /// 30-second write proportional to the visible grid, not to 20k
    /// lines of already-persisted history.  The alt fold section is
    /// NOT capped by this (its content exists nowhere on disk).
    pub fn serialize_snapshot_capped(&self, tail_cap: usize) -> Vec<u8> {
        self.serialize_snapshot_impl(tail_cap, false)
    }

    fn serialize_snapshot_impl(&self, tail_cap: usize, live: bool) -> Vec<u8> {
        // Main layer: in alt mode the real (File-backed) grid lives
        // in saved_main; the current grid is the alt shadow.
        let (main_grid, main_cursor, alt_grid) = match self.saved_main.as_ref() {
            Some(sm) => (&sm.grid, sm.cursor, Some(&self.grid)),
            None => (&self.grid, self.grid.cursor(), None),
        };
        let cols = main_grid.cols();
        let rows = main_grid.rows();
        // Pre-size: header (~50) + sc bookkeeping (~14) + cells.
        let mut out = Vec::with_capacity(64 + (cols as usize * rows as usize * CELL_BYTES));
        out.extend_from_slice(&SNAPSHOT_MAGIC.to_le_bytes());
        out.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
        out.extend_from_slice(&cols.to_le_bytes());
        out.extend_from_slice(&rows.to_le_bytes());
        let (cc, cr) = main_cursor;
        out.extend_from_slice(&cc.to_le_bytes());
        out.extend_from_slice(&cr.to_le_bytes());
        // Scroll region belongs to the CURRENT screen; after an alt
        // fold the restored state is main-mode, so emit full-screen
        // defaults when folding.
        let (st, sb) = if alt_grid.is_some() {
            (0u16, rows.saturating_sub(1))
        } else {
            (self.scroll_top, self.scroll_bot)
        };
        out.extend_from_slice(&st.to_le_bytes());
        out.extend_from_slice(&sb.to_le_bytes());
        // bits 0-4: DECCKM, bracketed paste, DECTCEM, pending wrap,
        // on the alternate screen; bits 15-19 are `ScreenModes::to_bits`
        let mut modes = self.cursor_key_application_mode as u32
            | (self.bracketed_paste_mode.on as u32) << 1
            | (self.cursor_visible as u32) << 2
            | (self.pending_wrap as u32) << 3
            | (self.saved_main.is_some() as u32) << 4
            | self.screen_modes.to_bits();
        // bits 5-6 = mouse_tracking_mode(0/1/2/3),bit 7 = SGR.
        // 0.6.66 起加,跨 execv 保留 DECSET 1000/1002/1003/1006 状态.
        // 之前老 image 跨 execv 不带这些 bits → 新 image 默认 Off,
        // claudecode 已经在 alt-screen 里,不会重发 ?1000h,wheel 跟
        // 着滚不动."只有少数 work" 就是这个症状.老 reader 不知道这
        // 些 bit,自然 default Off — backward-compat 平滑.
        let mtm_bits: u32 = match self.mouse_tracking_mode {
            MouseTrackingMode::Off => 0,
            MouseTrackingMode::X11 => 1,
            MouseTrackingMode::ButtonEvent => 2,
            MouseTrackingMode::AnyEvent => 3,
        };
        modes |= mtm_bits << 5;
        if self.mouse_sgr_encoding {
            modes |= 1 << 7;
        }
        // bit 8 = alt_scroll (DEC 1007).  Carried for the same reason
        // the mouse bits are: the program set it once, on entering a
        // view it is still in, and will not say it again.  An image
        // that came up without it would take the wheel back and — for
        // a pane whose plugin sends a TOGGLE to enter — press that
        // toggle on a view already open, shutting it.  An older reader
        // simply does not know the bit and gets the old default.
        if self.alt_scroll {
            modes |= 1 << 8;
        }
        // bit 9 = focus reporting (DEC 1004).  Same reason as the bits
        // above: the program set it once and will not say it again, so
        // an image that came up without it would stop answering a
        // question the program is still asking.
        if self.focus_reporting {
            modes |= 1 << 9;
        }
        // bit 10 = the G0 charset is DEC special graphics.  A pane that
        // is mid box-drawing when its process is replaced comes back
        // drawing boxes; without this it came back printing the letters
        // the line-drawing set is spelled with.  An older snapshot has
        // the bit clear, which is the charset it in fact had.
        if self.g0_graphics {
            modes |= 1 << 10;
        }
        // bit 11 = the charset the saved cursor is holding.  Bit 10 is
        // the live one; while a TUI is up, bit 10 is the TUI's and
        // bit 11 is what the shell gets back on `?1049l`.
        if self.saved_cursor.is_some_and(|c| c.g0_graphics) {
            modes |= 1 << 11;
        }
        // bits 12 and 13 = DECOM, live and as the saved cursor holds it.
        if self.origin_mode {
            modes |= 1 << 12;
        }
        // bit 14 = IRM.
        if self.insert_mode {
            modes |= 1 << 14;
        }
        if self.saved_cursor.is_some_and(|c| c.origin_mode) {
            modes |= 1 << 13;
        }
        out.extend_from_slice(&modes.to_le_bytes());
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.extend_from_slice(&serialize_attrs(self.attrs));
        if let Some(sc) = self.saved_cursor {
            out.push(1);
            out.extend_from_slice(&sc.col.to_le_bytes());
            out.extend_from_slice(&sc.row.to_le_bytes());
            out.extend_from_slice(&serialize_attrs(sc.attrs));
        } else {
            out.push(0);
        }
        for r in 0..rows {
            for c in 0..cols {
                let cell = main_grid.cell(c, r);
                out.extend_from_slice(&(cell.ch as u32).to_le_bytes());
                out.extend_from_slice(&serialize_attrs(cell.attrs));
            }
        }
        // v3 trailing scrollback section: take the most recent
        // `SNAPSHOT_SCROLLBACK_LINE_CAP` lines (or fewer if scrollback
        // is shorter) and write oldest-first so apply_snapshot can
        // push them back in arrival order.  Per-line: u8 wrapped flag,
        // u32 LE line_cols, then cells.  Section header has TWO
        // fields now: `start_logical_idx u64 LE` (the logical line
        // index of the first line emitted) + `take_n u32 LE` (count).
        // The reader uses start_idx to skip lines the on-disk
        // scrollback already has, so the buffered tail an L3 may
        // lose at execv gets refilled from the snapshot without
        // duplicating what's already persisted.  v1 readers see
        // start_idx as count and stop; that's wrong for v1 but v1 is
        // out of compat anyway (MIN_COMPAT bumps in lockstep when v3
        // becomes mandatory).  v2 readers skip the trailing section
        // entirely (they stop at the v2 count u32 and don't look
        // for more — the start_idx is consumed as their count, and
        // they exit cleanly because subsequent reads return None).
        // Wait — v2 receivers of v3 payloads is a hazard.  Defended
        // below: SNAPSHOT_MIN_COMPAT stays at 1 → the apply path
        // reads `snapshot_v`; v3 sender + v2 receiver is currently
        // impossible because every binary in flight bumped past v2
        // (image-swap protocol ensures matched versions in steady
        // state).  If a v2-receiver from a stale image ever does
        // apply a v3 payload, it'd misread the count field, but
        // the snapshot is regenerated on next push so the damage is
        // one render frame, not persistent state.
        let sb_len = main_grid.scrollback_len();
        let take_n = sb_len.min(tail_cap);
        let start_logical_idx = sb_len.saturating_sub(take_n) as u64;
        out.extend_from_slice(&start_logical_idx.to_le_bytes());
        out.extend_from_slice(&(take_n as u32).to_le_bytes());
        // line_idx convention: 0 = OLDEST, sb_len-1 = newest (just
        // above live) — verified empirically 2026-07-17 (RFC-004
        // B14).  Emit the NEWEST take_n lines in oldest-first order:
        // walk [sb_len - take_n, sb_len) ascending.  The previous
        // `(0..take_n).rev()` had the convention backwards on BOTH
        // axes — it took the OLDEST take_n lines in newest-first
        // order, so every v3 apply replayed the gap with copies of
        // the oldest history instead of the lost BufWriter tail
        // (length arithmetic matched, content didn't — the weak
        // `starts_with("line ")` assertions never caught it).
        for line_idx in (sb_len - take_n)..sb_len {
            // scrollback_read_page would also do this, but we want a
            // simpler per-line walk that doesn't allocate intermediate
            // Vecs — push the cells directly.
            if let Some(line) = main_grid.scrollback_line(line_idx) {
                // wrapped = the row continued from the previous (DECAWM
                // soft-wrap).  Read straight off `Grid::scrollback_wrapped`
                // which is kept in lockstep with `scrollback.push_line`
                // (see `push_historic_scrollback_line` + grid scroll_up).
                // Without this flag a cross-row URL gets chopped at the
                // row boundary in link scans after execv.
                let wrapped = main_grid.scrollback_wrapped(line_idx) as u8;
                out.push(wrapped);
                let line_cols = line.len() as u32;
                out.extend_from_slice(&line_cols.to_le_bytes());
                for cell in line.iter() {
                    out.extend_from_slice(&(cell.ch as u32).to_le_bytes());
                    out.extend_from_slice(&serialize_attrs(cell.attrs));
                }
            } else {
                // Scrollback shrank under us (rare); pad with an empty
                // line so the count we wrote still matches what reader
                // expects.
                out.push(0u8);
                out.extend_from_slice(&0u32.to_le_bytes());
            }
        }
        // v4 trailing alt-fold section: [alt_present u8]; when 1,
        // [count u32] + per-line (wrapped u8, cols u32, cells) —
        // the alt grid's ring history (oldest→newest) then its final
        // visible rows (trailing blank rows dropped).  These lines
        // exist ONLY in RAM (the alt grid is Memory-backed — B13),
        // so unlike the main tail they are never deduped on apply:
        // they simply append into scrollback and thereby reach disk.
        // Lines are right-trimmed (default-cell suffix dropped) —
        // Memory ring rows are stored at full grid width and a
        // claudecode transcript is mostly air.
        match alt_grid {
            None => out.push(ALT_SECTION_NONE),
            Some(alt) if live => {
                // v5 LIVE — verbatim alt screen for execv handoff.
                out.push(ALT_SECTION_LIVE);
                let (acc, acr) = alt.cursor();
                out.extend_from_slice(&alt.cols().to_le_bytes());
                out.extend_from_slice(&alt.rows().to_le_bytes());
                out.extend_from_slice(&acc.to_le_bytes());
                out.extend_from_slice(&acr.to_le_bytes());
                out.extend_from_slice(&self.scroll_top.to_le_bytes());
                out.extend_from_slice(&self.scroll_bot.to_le_bytes());
                let alt_sb_len = alt.scrollback_len();
                let ring_take = alt_sb_len.min(SNAPSHOT_SCROLLBACK_LINE_CAP);
                out.extend_from_slice(&(ring_take as u32).to_le_bytes());
                for line_idx in (alt_sb_len - ring_take)..alt_sb_len {
                    match alt.scrollback_line(line_idx) {
                        Some(line) => {
                            let trimmed = line
                                .iter()
                                .rposition(|c| *c != Cell::default())
                                .map(|i| i + 1)
                                .unwrap_or(0);
                            out.push(alt.scrollback_wrapped(line_idx) as u8);
                            out.extend_from_slice(&(trimmed as u32).to_le_bytes());
                            for cell in &line[..trimmed] {
                                out.extend_from_slice(&(cell.ch as u32).to_le_bytes());
                                out.extend_from_slice(&serialize_attrs(cell.attrs));
                            }
                        }
                        None => {
                            out.push(0u8);
                            out.extend_from_slice(&0u32.to_le_bytes());
                        }
                    }
                }
                for r in 0..alt.rows() {
                    for c in 0..alt.cols() {
                        let cell = alt.cell(c, r);
                        out.extend_from_slice(&(cell.ch as u32).to_le_bytes());
                        out.extend_from_slice(&serialize_attrs(cell.attrs));
                    }
                }
            }
            Some(alt) => {
                out.push(ALT_SECTION_FOLD);
                let ring_len = alt.scrollback_len().min(SNAPSHOT_SCROLLBACK_LINE_CAP);
                // Visible rows: drop trailing all-default rows.
                let arows = alt.rows();
                let acols = alt.cols();
                let mut last_content_row: i32 = -1;
                for r in 0..arows {
                    for c in 0..acols {
                        if alt.cell(c, r) != Cell::default() {
                            last_content_row = r as i32;
                            break;
                        }
                    }
                }
                let vis_rows = (last_content_row + 1) as usize;
                out.extend_from_slice(&((ring_len + vis_rows) as u32).to_le_bytes());
                let trim = |line: &[Cell]| -> usize {
                    line.iter()
                        .rposition(|c| *c != Cell::default())
                        .map(|i| i + 1)
                        .unwrap_or(0)
                };
                let emit = |line: &[Cell], wrapped: u8, out: &mut Vec<u8>| {
                    let n = trim(line);
                    out.push(wrapped);
                    out.extend_from_slice(&(n as u32).to_le_bytes());
                    for cell in &line[..n] {
                        out.extend_from_slice(&(cell.ch as u32).to_le_bytes());
                        out.extend_from_slice(&serialize_attrs(cell.attrs));
                    }
                };
                let mut scratch: Vec<Cell> = Vec::with_capacity(acols as usize);
                let alt_sb_len = alt.scrollback_len();
                for line_idx in (alt_sb_len - ring_len)..alt_sb_len {
                    match alt.scrollback_line(line_idx) {
                        Some(line) => {
                            let wrapped = alt.scrollback_wrapped(line_idx) as u8;
                            emit(&line[..], wrapped, &mut out);
                        }
                        None => {
                            out.push(0u8);
                            out.extend_from_slice(&0u32.to_le_bytes());
                        }
                    }
                }
                for r in 0..vis_rows {
                    scratch.clear();
                    for c in 0..acols {
                        scratch.push(alt.cell(c, r as u16));
                    }
                    emit(&scratch, 0, &mut out);
                }
            }
        }
        snapshot_clusters::append(&mut out, main_grid, alt_grid, live, tail_cap);
        out
    }
}
