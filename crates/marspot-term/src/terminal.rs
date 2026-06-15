//! Terminal emulator: bridges parsed VT events to grid mutations.
//!
//! `Terminal` owns a `Grid` (screen state) and a `Parser` (byte→event
//! state machine).  Bytes go in via `feed()`; the parser tokenizes them
//! and dispatches to `Handler`, which applies the semantic effect on the
//! grid (move cursor, print glyph, etc.).
//!
//! This module currently handles:
//! - Printable characters, CR, LF, BS routed to the corresponding `Grid` ops
//! - CSI cursor movement: CUU (A) / CUD (B) / CUF (C) / CUB (D) / CUP (H)
//!   / HVP (f) / HPA (G) / VPA (d)
//!
//! Phase 1.1.3+ will layer in erase, SGR attributes, scrolling, and more.

use crate::grid::{Cell, CellAttrs, Color, Grid, DEFAULT_SCROLLBACK_LINES};
use crate::parser::{Parser, ParserCallbacks};
use crate::scrollback::Scrollback;
use crate::{lx_debug, lx_debug_sampled, lx_info, lx_warn};
use std::collections::VecDeque;
use std::io;
use std::sync::OnceLock;
use std::time::Instant;

/// Whether disk-backed scrollback is on for this process.  Resolved
/// once on first call.  `MARSPOT_DISK_SCROLLBACK=0` opts out (RAM-only,
/// kept for regression bisects); any other value (or unset) gives
/// disk-on, the default since the anon-mmap rewrite landed.
///
/// The pre-anon-mmap implementation accepted a path here so the
/// scratch file's location was configurable; with anonymous mmap
/// there's no file, so the env var is binary now.  Old custom
/// paths are silently treated as "on".
fn disk_scrollback_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("MARSPOT_DISK_SCROLLBACK").as_deref() != Ok("0"))
}

/// In-RAM ring size when disk scrollback is active.  Front-line
/// cache for the most-recent N lines; older history goes through
/// the mmap'd ring.  Kept at 1024 deliberately:
///
/// Tried 4096 (Phase 3 of disk-scrollback default-on roadmap, see
/// `--bench scroll`) — the larger lazy-allocated `ram_cells` Vec
/// pays first-touch page faults on the parse hot path, costing
/// ~3 % cat-ascii throughput.  Scroll p99 didn't improve (mmap
/// region access is already as fast as Vec index), so the trade
/// failed: small idle-resident upside, measurable burst-output
/// downside.  Data on `feature/disk-scrollback-mmap` 2026-05-04.
const DISK_SCROLLBACK_RAM_LINES: usize = 1024;

/// Disk pages cap (each = 256 lines).  100 pages × 256 lines × 80
/// cols × 24 B/cell ≈ 50 MiB on-disk per session.  At 9 sessions
/// that's ~450 MiB on disk, well under macOS's reasonable cache
/// budget.  Bounded — file is fixed-size; oldest pages get
/// overwritten in place.
const DISK_SCROLLBACK_PAGES: usize = 100;

/// One outstanding local-echo prediction: a byte we expect the PTY
/// to echo back, plus the grid state we need to restore if it
/// mispredicts.
#[derive(Debug)]
struct Prediction {
    /// Byte we wrote to the PTY and expect to come back as echo.
    byte: u8,
    /// Cell at the predicted column at the moment we predicted —
    /// reinstated on rollback.
    saved_cell: Cell,
    /// Cursor before this prediction.  Rolling back N predictions
    /// in reverse restores the original cursor exactly.
    saved_cursor: (u16, u16),
}

pub struct Terminal {
    grid: Grid,
    /// Saved main grid while the terminal is in alt-screen mode (`?1049h`).
    /// `Some` ⇒ alt mode active and `grid` is the alt buffer; `None` ⇒
    /// normal mode and `grid` is the only buffer.  When we exit alt mode
    /// the saved cursor goes with this Grid.
    saved_main: Option<SavedMain>,
    parser: Parser,
    /// Current SGR state — every printed glyph (and every BCE-erased cell)
    /// is stamped with this snapshot.  Persists across `feed` calls.
    attrs: CellAttrs,
    /// Cursor + SGR snapshot saved by ESC 7 (DECSC) or CSI s (SCO save);
    /// restored by ESC 8 (DECRC) or CSI u (SCO restore). Cleared by neither —
    /// TUI apps like claudecode lean on save/restore to redraw progress
    /// regions in place. `None` until first save; restore with no prior
    /// save is a no-op (matches xterm).
    saved_cursor: Option<SavedCursor>,
    /// DECSTBM scroll region — `[scroll_top..=scroll_bot]` rows scroll
    /// together; rows outside this band stay put on LF / SU / IL / DL.
    /// Default `(0, rows-1)` = full grid (no region). Resize re-clamps.
    /// Used by TUI apps (claudecode footer, htop status line, vim
    /// statusline) to pin chrome below the scrolling content.
    scroll_top: u16,
    scroll_bot: u16,
    /// Bytes the terminal wants to send back to the PTY in response to
    /// a query (CSI c primary DA, CSI > 0 q XTQVERSION, etc.). Drained
    /// by `Session::pump` after each feed cycle and written to the PTY.
    /// Without this, TUI apps that query terminal capabilities at
    /// startup hang waiting for a response and fall back to degraded
    /// rendering (extra blank rows, misaligned chrome).
    pending_response: Vec<u8>,
    /// Rolling 100 ms window of capability-response timestamps so the
    /// forensic log can flag burst loops.  The 2026-06-15 incident saw
    /// every pane fill with literal `[?62;1;6;22c` text — DA1
    /// responses fed back into the PTY input by a stuck shell loop —
    /// and the surface symptom (cell content) couldn't point at a
    /// driver.  With this we emit one `term.respond.burst` WARN per
    /// session whenever the window crosses 5 responses, with the
    /// burst rate-limited to 1 / s so a sustained loop doesn't drown
    /// the log itself.  The window is intentionally per-Terminal —
    /// nine panes loop in parallel show up as nine warnings, which is
    /// what we want.
    response_window: VecDeque<Instant>,
    /// Last time we emitted a burst-warn, to rate-limit the WARNs.
    /// `None` if we've never warned (cold path).
    response_burst_last_warn: Option<Instant>,
    /// DEC mode `?1` (DECCKM): when set, cursor keys send the
    /// "application" sequence `ESC O X` instead of the normal
    /// `ESC [ X`. Some TUI apps toggle this to bind cursor keys
    /// distinctly from navigation. Read by the input layer when
    /// encoding arrow keys.
    cursor_key_application_mode: bool,
    /// DEC mode `?2004` (bracketed paste). When set, the input layer
    /// wraps Cmd-V paste content in `\e[200~ ... \e[201~` so the app
    /// (Claude Code TUI, vim, etc.) can distinguish pasted bytes from
    /// interactive typing. Without this, Claude Code's input handler
    /// treats each Chinese char in a paste as a separate keystroke and
    /// auto-inserts spaces between CJK characters — the "粘贴中文都
    /// 多了空格" symptom.
    bracketed_paste_mode: bool,
    /// DEC mode 25 (DECTCEM) — when false, the renderer hides the cursor.
    cursor_visible: bool,
    /// DECAWM "deferred wrap" — set after printing a glyph in the last
    /// column. The cursor visually stays put; the next print first wraps
    /// to a new row, the next non-print op (CR/LF/cursor move) clears
    /// the flag. Without this, drawing a box's right border followed by
    /// `\r\n` advances TWO rows instead of one — visible as extra blank
    /// rows between every row of TUI content (claudecode welcome box).
    pending_wrap: bool,
    /// Local-echo predictions awaiting PTY confirmation.  Each matching
    /// byte from `feed()` pops the front; the first mismatching byte
    /// rolls back the whole queue (restores cells + cursor in reverse
    /// order) and falls through to the parser.
    predictions: VecDeque<Prediction>,
    /// In-progress grapheme cluster (UAX #29).  The VT/xterm parser
    /// emits one codepoint at a time, but a "character the user sees"
    /// can span several — `é = e + ́`, `⚠️ = ⚠ + VS16`, `👨‍👩‍👧‍👦 = 4×
    /// emoji + 3× ZWJ`, `🇯🇵 = 2× RI`, `क्क = क + virama + क`.  The
    /// cluster_buf accumulates codepoints until the segmenter
    /// (`grapheme_cursor`) reports a boundary; on boundary or any
    /// non-print operation we compute cluster_width on the WHOLE
    /// buffered string and commit a single cell (with the cluster's
    /// base codepoint) at that width.  This is what makes ⭐ ✅ ❌
    /// land in 2 cells (their cluster_width is 2) instead of being
    /// half-clipped in a 1-cell slot.
    ///
    /// Phase 1 limitation (2026-06-15): only the cluster's base
    /// codepoint is stored in the cell — combining-mark glyphs and
    /// full compound-emoji glyphs are deferred to the cluster-pool
    /// + renderer-shaping work tracked under task #5.
    cluster_buf: String,
    grapheme_cursor: crate::grapheme::GraphemeCursor,
    /// Diagnostics — predictions confirmed by an echo byte.
    pub predictions_hit: u64,
    /// Diagnostics — predictions rolled back on mismatch (or alt-screen
    /// invalidation).
    pub predictions_miss: u64,
    /// RFC-002 snapshot version vector.  Bumped at the end of every
    /// `feed()` that consumed bytes.  The shelld snapshot slot keeps
    /// last-write-wins by this number; the client end uses it to
    /// reject stale `StateSnapshot` frames that arrive after newer
    /// live data has already landed.
    generation: u64,
}

struct SavedMain {
    grid: Grid,
    cursor: (u16, u16),
}

/// Snapshot captured by ESC 7 (DECSC) / CSI s (SCO save), restored by
/// ESC 8 (DECRC) / CSI u (SCO restore). Holds cursor position + the
/// SGR attrs at save time, since "save cursor" in DEC's spec covers
/// both attributes and origin-mode (we don't yet implement origin
/// mode; the field is reserved for that future addition).
#[derive(Clone, Copy)]
struct SavedCursor {
    col: u16,
    row: u16,
    attrs: CellAttrs,
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        // Disk-backed scrollback is default-on; set MARSPOT_DISK_SCROLLBACK=0
        // to opt out.  Falls back to the in-RAM ring on any mmap-init
        // error (no panic — the user just gets the bounded-RAM history).
        let scrollback = if disk_scrollback_enabled() {
            Scrollback::disk(
                DISK_SCROLLBACK_RAM_LINES,
                DISK_SCROLLBACK_PAGES,
                cols as usize,
            )
            .unwrap_or_else(|e| {
                eprintln!(
                    "[marspot] disk scrollback init failed ({e}); falling back to RAM-only"
                );
                Scrollback::memory(DEFAULT_SCROLLBACK_LINES, cols as usize)
            })
        } else {
            Scrollback::memory(DEFAULT_SCROLLBACK_LINES, cols as usize)
        };
        Self {
            grid: Grid::with_scrollback_kind(cols, rows, scrollback),
            saved_main: None,
            parser: Parser::new(),
            attrs: CellAttrs::default(),
            saved_cursor: None,
            scroll_top: 0,
            scroll_bot: rows.saturating_sub(1),
            pending_response: Vec::new(),
            response_window: VecDeque::new(),
            response_burst_last_warn: None,
            cursor_key_application_mode: false,
            bracketed_paste_mode: false,
            cursor_visible: true,
            pending_wrap: false,
            predictions: VecDeque::new(),
            cluster_buf: String::new(),
            grapheme_cursor: crate::grapheme::GraphemeCursor::new(),
            predictions_hit: 0,
            predictions_miss: 0,
            generation: 0,
        }
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// True when the terminal is in DECCKM application cursor key mode.
    /// Read by the input layer to encode arrow keys as `ESC O X`
    /// instead of `ESC [ X`. Required by TUI apps that bind cursor
    /// keys distinctly from PgUp/PgDn navigation.
    pub fn cursor_key_application_mode(&self) -> bool {
        self.cursor_key_application_mode
    }

    /// True when DECSET ?2004 is active — the app (Claude Code, vim,
    /// neovim, fish, zsh-bracketed-paste etc.) has asked the terminal
    /// to wrap pasted content in `\e[200~ ... \e[201~`. Read by the
    /// input layer when handling Cmd-V.
    pub fn bracketed_paste_mode(&self) -> bool {
        self.bracketed_paste_mode
    }

    /// Drain any bytes the terminal wants to send back to the PTY in
    /// response to capability / version queries (CSI c, CSI > 0 c,
    /// CSI > 0 q, …). Caller (Session::pump) writes them to the PTY
    /// after the feed cycle so the app's `read()` returns them.
    pub fn take_response(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_response)
    }


    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.grid.resize(cols, rows);
        // Reset DECSTBM scroll region to full grid on resize — apps
        // re-set their region when they reflow anyway (xterm behavior).
        // Saves us from having to clamp + worry about top > bot edge
        // cases when shrinking past a stored region.
        self.scroll_top = 0;
        self.scroll_bot = rows.saturating_sub(1);
        // The cursor may have been at the old last column; the new grid
        // width invalidates any pending wrap targeting that position.
        self.pending_wrap = false;
        // The saved main grid (when in alt-screen mode) needs to track
        // resizes too — otherwise leaving alt mode restores a grid
        // sized for the old window dimensions.
        if let Some(saved) = self.saved_main.as_mut() {
            saved.grid.resize(cols, rows);
            // Cursor was saved against the old size; clamp.
            saved.cursor.0 = saved.cursor.0.min(cols.saturating_sub(1));
            saved.cursor.1 = saved.cursor.1.min(rows.saturating_sub(1));
        }
    }

    pub fn current_attrs(&self) -> CellAttrs {
        self.attrs
    }

    /// True iff local-echo prediction is currently safe.  Heuristic:
    /// not in alt-screen mode (vim / less / htop don't echo keys
    /// verbatim).  Future: also peek at termios `ICANON|ECHO` via
    /// PTY ioctl when the bookkeeping cost is worth it.
    pub fn can_predict(&self) -> bool {
        self.saved_main.is_none()
    }

    /// Try to local-echo `byte`: if it's printable ASCII and we're
    /// in cooked-echo territory, write it to the grid + advance the
    /// cursor + queue a prediction so the matching PTY echo will be
    /// silently consumed.  Returns true when the prediction was
    /// applied (caller should request a redraw).
    ///
    /// We deliberately predict only `0x20..=0x7E`.  Other bytes have
    /// shell-side side effects we can't model from the keystroke
    /// alone: `\r` becomes `\r\n` under ONLCR, Tab triggers
    /// completion, Ctrl-* delivers signals, etc.
    pub fn predict_byte(&mut self, byte: u8) -> bool {
        if !self.can_predict() {
            return false;
        }
        if !(0x20..=0x7E).contains(&byte) {
            return false;
        }
        let cols = self.grid.cols();
        let (col, row) = self.grid.cursor();
        // Decline at-or-past the right edge — the wrap rule depends
        // on DECAWM and we don't track it precisely.
        if col >= cols {
            return false;
        }
        let saved_cell = self.grid.cell(col, row);
        let saved_cursor = (col, row);
        self.grid
            .set_cell(col, row, Cell { ch: byte as char, attrs: self.attrs });
        if col + 1 < cols {
            self.grid.set_cursor(col + 1, row);
        }
        // Cap the queue so a runaway typing session can't grow it
        // unbounded if echoes never come.  64 is generous — typical
        // round-trip is one byte before the next key.
        const MAX_PREDICTIONS: usize = 64;
        if self.predictions.len() >= MAX_PREDICTIONS {
            self.rollback_predictions();
            return false;
        }
        self.predictions.push_back(Prediction {
            byte,
            saved_cell,
            saved_cursor,
        });
        true
    }

    /// Reverse-pop every pending prediction, restoring its saved
    /// cell + cursor.  After this, the grid is back to whatever it
    /// looked like before the first un-confirmed prediction.
    fn rollback_predictions(&mut self) {
        while let Some(p) = self.predictions.pop_back() {
            self.grid
                .set_cell(p.saved_cursor.0, p.saved_cursor.1, p.saved_cell);
            self.grid.set_cursor(p.saved_cursor.0, p.saved_cursor.1);
            self.predictions_miss += 1;
        }
    }

    /// Feed bytes from the PTY through the parser, applying their effects
    /// to the grid.  Each byte is first checked against the front-of-queue
    /// prediction: a match silently consumes both (the prediction already
    /// painted the result), a mismatch rolls back the whole prediction
    /// queue and feeds the byte normally through the parser.
    pub fn feed(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // RFC-002: bump the generation vector once per non-empty feed
        // batch.  Coarse but sufficient — the snapshot pump (step 6)
        // compares against `last_pushed_generation` to decide whether
        // to push; a generation that hasn't moved means there's
        // nothing new for shelld to mirror.  saturating_add is
        // defensive against a u64 wrap that won't realistically
        // happen (would need ~600 years at 1 GHz).
        self.generation = self.generation.saturating_add(1);
        let mut i = 0;
        while i < bytes.len() {
            // Validate against pending predictions before the parser
            // sees the byte.
            if let Some(p) = self.predictions.front() {
                if bytes[i] == p.byte {
                    self.predictions.pop_front();
                    self.predictions_hit += 1;
                    i += 1;
                    continue;
                }
                self.rollback_predictions();
            }
            // Normal feed.
            let parser = &mut self.parser;
            let grid = &mut self.grid;
            let saved_main = &mut self.saved_main;
            let attrs = &mut self.attrs;
            let saved_cursor = &mut self.saved_cursor;
            let scroll_top = &mut self.scroll_top;
            let scroll_bot = &mut self.scroll_bot;
            let pending_response = &mut self.pending_response;
            let response_window = &mut self.response_window;
            let response_burst_last_warn = &mut self.response_burst_last_warn;
            let cursor_key_app_mode = &mut self.cursor_key_application_mode;
            let bracketed_paste = &mut self.bracketed_paste_mode;
            let cursor_visible = &mut self.cursor_visible;
            let pending_wrap = &mut self.pending_wrap;
            let cluster_buf = &mut self.cluster_buf;
            let grapheme_cursor = &mut self.grapheme_cursor;
            let mut handler = Handler {
                grid, saved_main, attrs, saved_cursor,
                scroll_top, scroll_bot,
                pending_response,
                response_window,
                response_burst_last_warn,
                cursor_key_app_mode,
                bracketed_paste,
                cursor_visible,
                pending_wrap,
                cluster_buf,
                grapheme_cursor,
            };
            parser.advance(&mut handler, bytes[i]);
            i += 1;

            // Entering alt-screen mid-feed swaps the grid wholesale —
            // any predictions queued beforehand were anchored to the
            // old grid and are now garbage.  Drop them.
            if !self.predictions.is_empty() && self.saved_main.is_some() {
                let n = self.predictions.len();
                self.predictions.clear();
                self.predictions_miss += n as u64;
            }
        }
        // End-of-feed flush: a trailing print(ch) leaves the cluster
        // in the buffer pending the next codepoint's break decision.
        // For interactive terminals each PTY write is its own feed
        // and the user expects the keystroke to land NOW, not on the
        // next read.  We don't reset the segmenter — a cluster split
        // across feed boundaries (rare; would need a partial UTF-8
        // run) still resolves via the segmenter's saved prev-state.
        //
        // Build a one-off handler so the flush method can do its work
        // through the same borrows as the per-byte handler.
        if !self.cluster_buf.is_empty() {
            let grid = &mut self.grid;
            let saved_main = &mut self.saved_main;
            let attrs = &mut self.attrs;
            let saved_cursor = &mut self.saved_cursor;
            let scroll_top = &mut self.scroll_top;
            let scroll_bot = &mut self.scroll_bot;
            let pending_response = &mut self.pending_response;
            let response_window = &mut self.response_window;
            let response_burst_last_warn = &mut self.response_burst_last_warn;
            let cursor_key_app_mode = &mut self.cursor_key_application_mode;
            let bracketed_paste = &mut self.bracketed_paste_mode;
            let cursor_visible = &mut self.cursor_visible;
            let pending_wrap = &mut self.pending_wrap;
            let cluster_buf = &mut self.cluster_buf;
            let grapheme_cursor = &mut self.grapheme_cursor;
            let mut handler = Handler {
                grid, saved_main, attrs, saved_cursor,
                scroll_top, scroll_bot,
                pending_response,
                response_window,
                response_burst_last_warn,
                cursor_key_app_mode,
                bracketed_paste,
                cursor_visible,
                pending_wrap,
                cluster_buf,
                grapheme_cursor,
            };
            handler.flush_cluster_keep_cursor();
        }
    }

    /// Current snapshot generation.  Bumped at the end of every non-
    /// empty `feed()` call.  RFC-002 step 6 / step 8 compare it
    /// against a `last_pushed_generation` to decide whether to push.
    pub fn generation(&self) -> u64 {
        self.generation
    }

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
    /// Alt-screen content (the `saved_main` shadow grid) is NOT
    /// included — restoring to alt mode in the middle of a vim
    /// session is a separate problem (UX-level: the user expects
    /// to re-open vim, not have it half-resumed).
    pub fn serialize_snapshot(&self) -> Vec<u8> {
        let cols = self.grid.cols();
        let rows = self.grid.rows();
        // Pre-size: header (~50) + sc bookkeeping (~14) + cells.
        let mut out = Vec::with_capacity(64 + (cols as usize * rows as usize * CELL_BYTES));
        out.extend_from_slice(&SNAPSHOT_MAGIC.to_le_bytes());
        out.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
        out.extend_from_slice(&cols.to_le_bytes());
        out.extend_from_slice(&rows.to_le_bytes());
        let (cc, cr) = self.grid.cursor();
        out.extend_from_slice(&cc.to_le_bytes());
        out.extend_from_slice(&cr.to_le_bytes());
        out.extend_from_slice(&self.scroll_top.to_le_bytes());
        out.extend_from_slice(&self.scroll_bot.to_le_bytes());
        let mut modes: u32 = 0;
        if self.cursor_key_application_mode { modes |= 1 << 0; }
        if self.bracketed_paste_mode        { modes |= 1 << 1; }
        if self.cursor_visible              { modes |= 1 << 2; }
        if self.pending_wrap                { modes |= 1 << 3; }
        if self.saved_main.is_some()        { modes |= 1 << 4; }
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
                let cell = self.grid.cell(c, r);
                out.extend_from_slice(&(cell.ch as u32).to_le_bytes());
                out.extend_from_slice(&serialize_attrs(cell.attrs));
            }
        }
        out
    }

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
        if snapshot_v != SNAPSHOT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported snapshot version: {}", snapshot_v),
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
            Some(SavedCursor { col, row, attrs: sc_attrs })
        } else {
            None
        };
        let want_cells = cols as usize * rows as usize;
        let mut cells = Vec::with_capacity(want_cells);
        for _ in 0..want_cells {
            let ch_u = read_u32(&mut cur)?;
            let cell_attrs = read_attrs(&mut cur)?;
            let ch = char::from_u32(ch_u).unwrap_or(' ');
            cells.push(Cell { ch, attrs: cell_attrs });
        }
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
        self.bracketed_paste_mode        = (modes & (1 << 1)) != 0;
        self.cursor_visible              = (modes & (1 << 2)) != 0;
        self.pending_wrap                = (modes & (1 << 3)) != 0;
        // Bit 4 (in_alt_screen) is informational for the wire format
        // but not actionable here — apply_snapshot replaces the
        // current grid; alt-mode save state is regenerated on the
        // next `?1049h` toggle from the PTY stream.
        self.attrs = attrs;
        self.saved_cursor = saved_cursor;
        self.generation = generation;
        // Reset transient state so a half-feed cluster / prediction
        // queue / response buffer doesn't bleed across the snapshot.
        self.predictions.clear();
        self.cluster_buf.clear();
        self.grapheme_cursor = crate::grapheme::GraphemeCursor::new();
        self.pending_response.clear();
        Ok(())
    }

    /// RFC-002 §8: serialize a contiguous slice of the scrollback into
    /// the `ScrollbackPage` body format.
    ///
    /// `line_start` indexes the scrollback ring from the oldest live
    /// line (0 = bottom of history, i.e. just above the visible grid
    /// is the highest index — same convention as `Grid::scrollback_*`).
    /// `count` is a request cap; the returned `line_count` is the
    /// number of lines actually serialised (may be < count when the
    /// request hits the end of scrollback).
    ///
    /// Wire format (per line — outer framing handled by
    /// `shelld_proto::encode_scrollback_page`):
    ///
    /// ```text
    /// repeat line_count times:
    ///   [line_cols u32 LE]                — width of THIS line (lines
    ///                                       may differ when the grid
    ///                                       was resized)
    ///   [cells: line_cols × 13 bytes]     — same Cell encoding as
    ///                                       `serialize_snapshot`
    /// ```
    pub fn serialize_scrollback_page(
        &self,
        line_start: u32,
        count: u32,
    ) -> (u32, Vec<u8>) {
        let lines = self.grid.scrollback_read_page(
            line_start as usize,
            count as usize,
        );
        let line_count = lines.len() as u32;
        let body_bytes: usize = lines
            .iter()
            .map(|l| 4 + l.len() * CELL_BYTES)
            .sum();
        let mut out = Vec::with_capacity(body_bytes);
        for line in &lines {
            out.extend_from_slice(&(line.len() as u32).to_le_bytes());
            for cell in line {
                out.extend_from_slice(&(cell.ch as u32).to_le_bytes());
                out.extend_from_slice(&serialize_attrs(cell.attrs));
            }
        }
        (line_count, out)
    }

    /// RFC-002 §8 (step 8c): push a historic line (chronologically
    /// older than every line already in scrollback) into the tail of
    /// the scrollback ring.  Thin wrapper around
    /// `Grid::push_historic_scrollback_line`; same ordering caveat —
    /// caller must apply history in oldest-first order before any
    /// live data has scrolled off the visible grid.
    pub fn push_historic_line(&mut self, line: &[Cell]) {
        self.grid.push_historic_scrollback_line(line);
    }

    /// Inverse of `serialize_scrollback_page`.  Static — no `&self`
    /// because the decoder doesn't touch terminal state; callers
    /// (L3 publish-cache) decide what to do with the lines.
    pub fn decode_scrollback_page_body(
        line_count: u32,
        body: &[u8],
    ) -> io::Result<Vec<Vec<Cell>>> {
        let mut cur = Cursor::new(body);
        let mut out = Vec::with_capacity(line_count as usize);
        for _ in 0..line_count {
            let line_cols = read_u32(&mut cur)? as usize;
            let mut line = Vec::with_capacity(line_cols);
            for _ in 0..line_cols {
                let ch_u = read_u32(&mut cur)?;
                let attrs = read_attrs(&mut cur)?;
                let ch = char::from_u32(ch_u).unwrap_or(' ');
                line.push(Cell { ch, attrs });
            }
            out.push(line);
        }
        Ok(out)
    }
}

// ─── Snapshot wire format helpers ─────────────────────────────────────

use std::io::Cursor;

const SNAPSHOT_MAGIC: u32 = 0xA557_5301;
const SNAPSHOT_VERSION: u32 = 1;
const ATTRS_BYTES: usize = 9;
const CELL_BYTES: usize = 4 + ATTRS_BYTES;

fn serialize_attrs(a: CellAttrs) -> [u8; ATTRS_BYTES] {
    let mut flags = 0u8;
    if a.bold      { flags |= 1 << 0; }
    if a.italic    { flags |= 1 << 1; }
    if a.underline { flags |= 1 << 2; }
    if a.reverse   { flags |= 1 << 3; }
    if a.dim       { flags |= 1 << 4; }
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
    match c {
        Color::Default => (0, [0, 0, 0]),
        Color::Indexed(i) => (1, [i, 0, 0]),
        Color::Rgb(r, g, b) => (2, [r, g, b]),
    }
}

fn decode_color(kind: u8, payload: [u8; 3]) -> io::Result<Color> {
    Ok(match kind {
        0 => Color::Default,
        1 => Color::Indexed(payload[0]),
        2 => Color::Rgb(payload[0], payload[1], payload[2]),
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown color kind {}", other),
            ));
        }
    })
}

fn read_attrs(cur: &mut Cursor<&[u8]>) -> io::Result<CellAttrs> {
    let mut buf = [0u8; ATTRS_BYTES];
    use std::io::Read;
    cur.read_exact(&mut buf)?;
    let flags = buf[0];
    let fg_kind = buf[1];
    let fg_payload = [buf[2], buf[3], buf[4]];
    let bg_kind = buf[5];
    let bg_payload = [buf[6], buf[7], buf[8]];
    Ok(CellAttrs {
        bold:      (flags & (1 << 0)) != 0,
        italic:    (flags & (1 << 1)) != 0,
        underline: (flags & (1 << 2)) != 0,
        reverse:   (flags & (1 << 3)) != 0,
        dim:       (flags & (1 << 4)) != 0,
        fg: decode_color(fg_kind, fg_payload)?,
        bg: decode_color(bg_kind, bg_payload)?,
    })
}

fn read_u8(cur: &mut Cursor<&[u8]>) -> io::Result<u8> {
    let mut b = [0u8; 1];
    use std::io::Read;
    cur.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u16(cur: &mut Cursor<&[u8]>) -> io::Result<u16> {
    let mut b = [0u8; 2];
    use std::io::Read;
    cur.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32(cur: &mut Cursor<&[u8]>) -> io::Result<u32> {
    let mut b = [0u8; 4];
    use std::io::Read;
    cur.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(cur: &mut Cursor<&[u8]>) -> io::Result<u64> {
    let mut b = [0u8; 8];
    use std::io::Read;
    cur.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

struct Handler<'a> {
    grid: &'a mut Grid,
    saved_main: &'a mut Option<SavedMain>,
    attrs: &'a mut CellAttrs,
    saved_cursor: &'a mut Option<SavedCursor>,
    scroll_top: &'a mut u16,
    scroll_bot: &'a mut u16,
    pending_response: &'a mut Vec<u8>,
    response_window: &'a mut VecDeque<Instant>,
    response_burst_last_warn: &'a mut Option<Instant>,
    cursor_key_app_mode: &'a mut bool,
    bracketed_paste: &'a mut bool,
    cursor_visible: &'a mut bool,
    pending_wrap: &'a mut bool,
    cluster_buf: &'a mut String,
    grapheme_cursor: &'a mut crate::grapheme::GraphemeCursor,
}

impl<'a> Handler<'a> {
    /// See `Terminal::record_response` — same logic, but borrowed
    /// fields instead of `&mut self`.  Called from the three CSI
    /// `c`/`>c`/`>q` dispatch arms where a capability response gets
    /// queued.  Forensic: a burst of these in a 100 ms window means a
    /// TUI is in a query-response loop (the 2026-06-15 incident
    /// symptom), which we'd otherwise see only as cell content.
    fn record_response(&mut self, kind: &'static str) {
        lx_debug!("term.respond", kind);
        let now = Instant::now();
        let cutoff = now - std::time::Duration::from_millis(100);
        while self
            .response_window
            .front()
            .map(|t| *t < cutoff)
            .unwrap_or(false)
        {
            self.response_window.pop_front();
        }
        self.response_window.push_back(now);
        if self.response_window.len() >= 5 {
            let warn_ok = self
                .response_burst_last_warn
                .map(|t| now.duration_since(t) >= std::time::Duration::from_secs(1))
                .unwrap_or(true);
            if warn_ok {
                let count = self.response_window.len();
                lx_warn!(
                    "term.respond.burst",
                    "capability-response storm — likely echo loop",
                    count = count,
                    window_ms = 100,
                    kind = kind
                );
                *self.response_burst_last_warn = Some(now);
            }
        }
    }
}

impl<'a> Handler<'a> {
    /// Commit the buffered grapheme cluster to the grid: compute its
    /// width over the WHOLE buffered string (so VS16, ZWJ glue, RI
    /// pairs, and combining marks all factor in), write the cluster's
    /// base codepoint to the cell at the cursor with that width, and
    /// clear the buffer.  Does NOT touch the segmenter state — call
    /// after a `step` returned `true` and you've already seeded the
    /// state with the next cluster's first codepoint.
    ///
    /// Phase 1 limitation: only the base codepoint is committed to
    /// the cell.  Full-cluster glyph rendering (compound emoji,
    /// combining marks) lands when we move cluster storage into a
    /// Grid-side pool (task #5 follow-up).
    fn flush_cluster_keep_cursor(&mut self) {
        if self.cluster_buf.is_empty() {
            return;
        }
        let w = crate::grapheme::cluster_width(self.cluster_buf);
        let base = self
            .cluster_buf
            .chars()
            .next()
            .expect("non-empty buffer");
        self.cluster_buf.clear();
        if w > 0 {
            self.write_glyph(base, w);
        }
    }

    /// Same as [`flush_cluster_keep_cursor`] but also resets the
    /// segmenter — the next codepoint will be treated as a fresh
    /// cluster start.  Use at every non-print event (control byte,
    /// escape sequence, end-of-feed) so a cursor move or CSI doesn't
    /// fuse two visually distinct clusters across the operation.
    fn flush_cluster_for_break(&mut self) {
        self.flush_cluster_keep_cursor();
        self.grapheme_cursor.reset();
    }

    /// Commit one already-segmented glyph (codepoint + cell width) to
    /// the grid at the cursor, handling the DECAWM deferred-wrap and
    /// wide-char wrap edge cases.  Body extracted from the old
    /// per-codepoint `print` so the cluster flush path and any future
    /// non-parser writer can share the same cursor-advance logic.
    fn write_glyph(&mut self, ch: char, w: u8) {
        // DECAWM deferred wrap: the previous glyph landed in the last
        // column and set `pending_wrap`. The wrap was deliberately
        // deferred so that a trailing `\r\n` (or any cursor move)
        // wouldn't compound with the wrap into a two-row advance —
        // the classic "every row has a blank row after it" symptom
        // when TUIs draw box borders flush against the right edge.
        self.take_pending_wrap();

        let cols = self.grid.cols();
        let rows = self.grid.rows();
        let (mut col, mut row) = self.grid.cursor();

        // A wide glyph at the last column can't fit. Wrap first, then
        // write at the start of the new row.
        if w == 2 && col + 1 >= cols {
            // The abandoned last column gets a NUL pad sentinel (when
            // it isn't carrying real content) so resize reflow knows
            // it's wide-wrap padding, not a space the user typed.
            if self.grid.cell(cols - 1, row) == Cell::default() {
                self.grid.set_cell(
                    cols - 1,
                    row,
                    Cell { ch: '\0', attrs: *self.attrs },
                );
            }
            let bot = *self.scroll_bot;
            if row == bot {
                self.region_scroll_up(1);
                self.grid.set_cursor(0, row);
            } else if row + 1 < rows {
                self.grid.set_cursor(0, row + 1);
            } else {
                self.grid.set_cursor(0, rows - 1);
            }
            let next = self.grid.cursor();
            col = next.0;
            row = next.1;
            self.grid.set_row_wrapped(row, true);
        }

        // Lead cell carries the printable char.  For wide glyphs, the
        // trail cell stores NUL with the same attrs — the renderer
        // skips drawing its glyph (NUL is treated as blank), and the
        // lead glyph extends visually across both cells via its natural
        // advance width.
        self.grid.set_cell(col, row, Cell { ch, attrs: *self.attrs });
        if w == 2 {
            self.grid.set_cell(col + 1, row, Cell { ch: '\0', attrs: *self.attrs });
        }

        let next_col = col + w as u16;
        if next_col < cols {
            self.grid.set_cursor(next_col, row);
        } else {
            // Hit the right edge — defer the wrap. Cursor visually
            // stays at the last column; the next write will consume
            // the flag and wrap, any non-print op clears it without
            // advancing.
            self.grid.set_cursor(cols - 1, row);
            *self.pending_wrap = true;
        }
    }

    /// Scroll the grid up by 1 line, honouring DECSTBM. When the
    /// scroll region covers the whole grid (the default) this drops
    /// to `grid.scroll_up` which also pushes to scrollback —
    /// region-bounded scrolls don't push to scrollback (they're
    /// in-grid only, e.g. TUI footer redraws).
    fn region_scroll_up(&mut self, lines: u16) {
        let top = *self.scroll_top;
        let bot = *self.scroll_bot;
        let rows = self.grid.rows();
        if top == 0 && bot + 1 >= rows {
            self.grid.scroll_up(lines, blank_with(*self.attrs));
        } else {
            self.grid.scroll_up_region(top, bot, lines, blank_with(*self.attrs));
        }
    }

    fn region_scroll_down(&mut self, lines: u16) {
        let top = *self.scroll_top;
        let bot = *self.scroll_bot;
        self.grid.scroll_down_region(top, bot, lines, blank_with(*self.attrs));
    }

    /// Consume the DECAWM "deferred wrap" flag (set by print at the
    /// last column). When set, advance the cursor to the start of the
    /// next row, scrolling within the region if at the bottom — same
    /// path the immediate-wrap branch used to take. Called at the top
    /// of `print` before drawing the next glyph; cleared without
    /// advancing by any non-print operation.
    fn take_pending_wrap(&mut self) {
        if !*self.pending_wrap {
            return;
        }
        *self.pending_wrap = false;
        let (_col, row) = self.grid.cursor();
        let rows = self.grid.rows();
        if row == *self.scroll_bot {
            self.region_scroll_up(1);
            self.grid.set_cursor(0, row);
        } else if row + 1 < rows {
            self.grid.set_cursor(0, row + 1);
        } else {
            self.grid.set_cursor(0, rows - 1);
        }
        // The row the cursor just flowed onto continues the logical
        // line above — record it so resize can re-wrap instead of
        // truncating.  (For the region-scroll branch the cursor row
        // index is unchanged but the content shifted up; the flag
        // still describes "this row continues the one above it".)
        let (_c, new_row) = self.grid.cursor();
        self.grid.set_row_wrapped(new_row, true);
    }
}

impl<'a> Handler<'a> {
    /// `?1049h` — switch to a fresh alternate screen, save the main
    /// grid + cursor.  No-op if already in alt mode.
    fn enter_alt_screen(&mut self) {
        if self.saved_main.is_some() {
            return;
        }
        let cols = self.grid.cols();
        let rows = self.grid.rows();
        let cursor = self.grid.cursor();
        // Alt buffer never needs scrollback — its job is to be discarded
        // wholesale on `?1049l`.  Skip the allocation.
        let alt = Grid::with_scrollback(cols, rows, 0);
        let main = std::mem::replace(self.grid, alt);
        *self.saved_main = Some(SavedMain { grid: main, cursor });
    }

    /// `?1049l` — restore the saved main grid + cursor.  No-op if not
    /// in alt mode.
    fn exit_alt_screen(&mut self) {
        if let Some(saved) = self.saved_main.take() {
            *self.grid = saved.grid;
            self.grid.set_cursor(saved.cursor.0, saved.cursor.1);
        }
    }

    fn dec_mode(&mut self, mode: u16, set: bool) {
        match mode {
            // DECCKM — cursor keys send `ESC O X` in app mode, `ESC [ X`
            // otherwise. Read by the input layer for arrow encoding.
            1 => *self.cursor_key_app_mode = set,
            // DECTCEM — cursor visibility.
            25 => *self.cursor_visible = set,
            // smcup/rmcup — alt screen + save/restore cursor.  ?1047
            // and ?47 are older variants; we accept them as aliases.
            1049 | 1047 | 47 => {
                if set {
                    self.enter_alt_screen();
                } else {
                    self.exit_alt_screen();
                }
            }
            // DECSET ?2004 — bracketed paste. When set, the input
            // layer wraps Cmd-V paste in `\e[200~ ... \e[201~` so apps
            // can distinguish paste from interactive typing. Read via
            // `Terminal::bracketed_paste_mode()`.
            2004 => *self.bracketed_paste = set,
            // Accept silently — these modes have no rendering side
            // effect we model, but apps want them to "succeed" rather
            // than no-op silently. Listed explicitly so future audits
            // see them.
            //   1000 / 1002 / 1003 / 1006 / 1015 — mouse reporting modes
            //   1004                — focus reporting in/out events
            //   2026                — synchronized output (begin/end batch)
            //   2031                — color scheme update notifications
            1000 | 1002 | 1003 | 1006 | 1015 | 1004 | 2026 | 2031 => {}
            _ => {} // unhandled DEC private mode — silently skip
        }
    }
}

impl<'a> ParserCallbacks for Handler<'a> {
    fn print(&mut self, ch: char) {
        // UAX #29 cluster aware: the VT parser feeds us one codepoint
        // at a time, but a single user-perceived "character" can span
        // several (é = e + ́, ⚠️ = ⚠ + VS16, 👨‍👩‍👧‍👦 = 4 emoji + 3
        // ZWJ, क्क = क + virama + क …).  We buffer codepoints, ask
        // the segmenter whether a boundary falls before each one, and
        // commit a single cluster to the grid when the next codepoint
        // starts a new one.  This is what makes ⭐ ✅ ❌ land in 2
        // cells (cluster_width=2) instead of being half-clipped in a
        // 1-cell slot when the EAW table alone gave them 1.
        if self.grapheme_cursor.step(ch) {
            // step has already advanced cursor state to track `ch` as
            // the first codepoint of a new cluster — flush_cluster
            // therefore must NOT reset the cursor, or the run state
            // would lose its head.
            self.flush_cluster_keep_cursor();
        }
        self.cluster_buf.push(ch);
    }

    fn execute(&mut self, byte: u8) {
        // Any pending grapheme cluster ends here: a C0 control byte
        // can never extend a cluster, so commit it now.
        self.flush_cluster_for_break();
        // Trace C0 row-advancers (LF/CR/BS/Tab) so we can see how the
        // app actually moves between rows — `CSI 1 B` shows up in the
        // CSI trace but plain `\n` / `\r` only show here.
        if matches!(byte, 0x08 | 0x09 | 0x0A | 0x0B | 0x0C | 0x0D) {
            trace_seq("C0", &[], &[], byte);
        }
        // Any C0 control cancels DECAWM deferred wrap without advancing.
        *self.pending_wrap = false;
        match byte {
            0x08 => {
                // BS: cursor left one column, clamped at column 0.  Does
                // not erase the cell.
                let (col, row) = self.grid.cursor();
                if col > 0 {
                    self.grid.set_cursor(col - 1, row);
                }
            }
            0x0A | 0x0B | 0x0C => {
                // LF / VT / FF: cursor down one row, scrolling at the
                // bottom of the scroll region.  Does not change column
                // (LNM mode unset).
                let (col, row) = self.grid.cursor();
                let rows = self.grid.rows();
                let bot = *self.scroll_bot;
                if row == bot {
                    // At scroll-region bottom — scroll within region.
                    self.region_scroll_up(1);
                    // Cursor stays at the now-blank last row.
                } else if row + 1 < rows {
                    // Anywhere else: move cursor down.
                    self.grid.set_cursor(col, row + 1);
                }
                // If row+1 == rows but row != bot (cursor outside region),
                // xterm-style: cursor caps, no scroll.
            }
            0x0D => {
                // CR: cursor to column 0 of current row.
                let (_col, row) = self.grid.cursor();
                self.grid.set_cursor(0, row);
            }
            0x09 => {} // TAB — tab stops land in a later phase
            0x07 => {} // BEL — visible bell deferred
            _ => {}    // unknown C0 control: ignore for now
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], byte: u8) {
        self.flush_cluster_for_break();
        trace_seq("ESC", intermediates, &[], byte);
        *self.pending_wrap = false;
        match byte {
            // DECSC — save cursor (position + SGR attrs).
            b'7' => {
                let (col, row) = self.grid.cursor();
                *self.saved_cursor = Some(SavedCursor { col, row, attrs: *self.attrs });
            }
            // DECRC — restore cursor. xterm-style no-op when no save exists.
            b'8' => {
                if let Some(s) = *self.saved_cursor {
                    self.grid.set_cursor(s.col, s.row);
                    *self.attrs = s.attrs;
                }
            }
            // DECKPAM (=) / DECKPNM (>) — application / normal keypad
            // mode. Same input-encoding category as DECCKM; accept
            // silently for now (the keypad-specific keys we encode
            // don't yet distinguish modes).
            b'=' | b'>' => {}
            // ESC ( <c> — designate G0 charset. We're always ASCII so
            // every variant is a no-op.
            _ if intermediates == b"(" => {}
            // RIS / charset switching / etc. arrive in later phases.
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &[u16], intermediates: &[u8], byte: u8) {
        self.flush_cluster_for_break();
        trace_seq("CSI", intermediates, params, byte);
        // Every CSI through the parser lands a sampled DEBUG line.
        // 1/64 keeps cost flat under heavy apps (tmux paints ~hundreds
        // of CSI/s), but bursts above that get truncated logarithmically
        // — good enough to spot patterns ("this app drives ~50 CUP/s")
        // without flooding when an SGR-heavy app like btop runs.  Set
        // `MARSPOT_LOG_TERM=debug` to enable; default INFO is 0 ns.
        lx_debug_sampled!(
            "term.csi",
            64,
            "CSI dispatch",
            final_byte = byte as char,
            intermediates_len = intermediates.len(),
            param0 = params.first().copied().unwrap_or(0)
        );
        *self.pending_wrap = false;
        if intermediates == b"?" {
            // DEC private mode set/reset.  Each param is a separate mode.
            match byte {
                b'h' => {
                    for &p in params {
                        lx_debug!("term.mode.set", "DECSET", mode = p);
                        self.dec_mode(p, true);
                    }
                }
                b'l' => {
                    for &p in params {
                        lx_debug!("term.mode.set", "DECRST", mode = p);
                        self.dec_mode(p, false);
                    }
                }
                other => {
                    lx_debug!(
                        "term.csi.unsupported",
                        "DEC private CSI with unhandled final",
                        final_byte = other as char
                    );
                }
            }
            return;
        }
        if !intermediates.is_empty() {
            // Other private-marker sequences not implemented yet.
            lx_debug!(
                "term.csi.unsupported",
                "CSI with non-? intermediate",
                final_byte = byte as char,
                int_count = intermediates.len()
            );
            return;
        }
        let (col, row) = self.grid.cursor();
        let cols = self.grid.cols();
        let rows = self.grid.rows();
        match byte {
            b'A' => {
                // CUU: cursor up by N (default 1).
                let n = param(params, 0, 1);
                self.grid.set_cursor(col, row.saturating_sub(n));
            }
            b'B' => {
                // CUD: cursor down by N.  set_cursor clamps at rows-1.
                let n = param(params, 0, 1);
                self.grid.set_cursor(col, row.saturating_add(n).min(rows - 1));
            }
            b'C' => {
                // CUF: cursor forward (right).
                let n = param(params, 0, 1);
                self.grid.set_cursor(col.saturating_add(n).min(cols - 1), row);
            }
            b'D' => {
                // CUB: cursor back (left).
                let n = param(params, 0, 1);
                self.grid.set_cursor(col.saturating_sub(n), row);
            }
            b'H' | b'f' => {
                // CUP / HVP: cursor position (1-indexed row;col → 0-indexed).
                let r = param(params, 0, 1).saturating_sub(1);
                let c = param(params, 1, 1).saturating_sub(1);
                self.grid.set_cursor(c, r);
            }
            b'G' => {
                // HPA: horizontal position absolute (1-indexed).
                let c = param(params, 0, 1).saturating_sub(1);
                self.grid.set_cursor(c, row);
            }
            b'd' => {
                // VPA: vertical position absolute (1-indexed).
                let r = param(params, 0, 1).saturating_sub(1);
                self.grid.set_cursor(col, r);
            }
            b'J' => {
                // ED: erase in display.  Cursor is not moved.
                // 0 (default) — from cursor (inclusive) to end of screen
                // 1           — from start of screen to cursor (inclusive)
                // 2           — entire screen
                // 3           — entire scrollback (xterm extension)
                let mode = param_raw(params, 0, 0);
                lx_debug_sampled!(
                    "term.edit.ED", 4,
                    "erase in display",
                    mode = mode,
                    cur_col = col,
                    cur_row = row,
                    cols = cols,
                    rows = rows
                );
                if mode == 3 {
                    self.grid.clear_scrollback();
                } else {
                    erase_in_display(self.grid, col, row, cols, rows, mode, *self.attrs);
                    if mode == 2 {
                        // Whole screen wiped — continuation flags no
                        // longer describe anything; without this a
                        // post-clear redraw would reflow-glue onto
                        // pre-clear history.
                        self.grid.clear_all_wrapped();
                    }
                }
            }
            b'K' => {
                // EL: erase in line.  Cursor not moved.
                // 0 (default) — from cursor (inclusive) to end of line
                // 1           — from start of line to cursor (inclusive)
                // 2           — entire line
                let mode = param_raw(params, 0, 0);
                lx_debug_sampled!(
                    "term.edit.EL", 4,
                    "erase in line",
                    mode = mode,
                    cur_col = col,
                    cur_row = row,
                    cols = cols
                );
                erase_in_line(self.grid, col, row, cols, mode, *self.attrs);
            }
            b'm' => {
                // SGR: set graphic rendition.  Mutates self.attrs in place.
                apply_sgr(self.attrs, params);
            }
            b's' => {
                // SCO save cursor.  Same semantics as DECSC (ESC 7).
                let (col, row) = self.grid.cursor();
                *self.saved_cursor = Some(SavedCursor { col, row, attrs: *self.attrs });
            }
            b'u' => {
                // SCO restore cursor.  Same semantics as DECRC (ESC 8).
                if let Some(s) = *self.saved_cursor {
                    self.grid.set_cursor(s.col, s.row);
                    *self.attrs = s.attrs;
                }
            }
            b'r' => {
                // DECSTBM: set top + bottom margins of the scroll region.
                // Params are 1-indexed inclusive. Default is full grid.
                // Cursor moves to (0, 0) (xterm behavior).
                let rows = self.grid.rows();
                let top = param(params, 0, 1).saturating_sub(1);
                let bot_arg = param_raw(params, 1, 0);
                let bot = if bot_arg == 0 {
                    rows.saturating_sub(1)
                } else {
                    bot_arg.saturating_sub(1).min(rows.saturating_sub(1))
                };
                // xterm: invalid region (top >= bot, or bot >= rows) is
                // ignored. We accept top == bot (1-row region) since
                // claudecode and some apps actually use that.
                if top <= bot && bot < rows {
                    *self.scroll_top = top;
                    *self.scroll_bot = bot;
                }
                self.grid.set_cursor(0, 0);
            }
            b'L' => {
                // IL: insert N blank lines at cursor row, within scroll
                // region. Rows below shift down; rows past scroll_bot
                // stay put. Cursor moves to col 0 of the same row.
                let n = param(params, 0, 1);
                lx_info!(
                    "term.edit.IL",
                    "insert lines",
                    n = n,
                    cur_row = row,
                    scroll_top = *self.scroll_top,
                    scroll_bot = *self.scroll_bot
                );
                if row >= *self.scroll_top && row <= *self.scroll_bot {
                    // Use a sub-region [row..=scroll_bot] for the shift.
                    self.grid.scroll_down_region(row, *self.scroll_bot, n, blank_with(*self.attrs));
                    self.grid.set_cursor(0, row);
                }
            }
            b'M' => {
                // DL: delete N lines at cursor row, within scroll
                // region. Rows below shift up; rows past scroll_bot
                // stay put. Cursor moves to col 0 of the same row.
                let n = param(params, 0, 1);
                lx_info!(
                    "term.edit.DL",
                    "delete lines",
                    n = n,
                    cur_row = row,
                    scroll_top = *self.scroll_top,
                    scroll_bot = *self.scroll_bot
                );
                if row >= *self.scroll_top && row <= *self.scroll_bot {
                    self.grid.scroll_up_region(row, *self.scroll_bot, n, blank_with(*self.attrs));
                    self.grid.set_cursor(0, row);
                }
            }
            b'@' => {
                // ICH: insert N blank chars at cursor — shift cells
                // [col..cols-n] right to [col+n..cols], blank [col..col+n].
                let n = param(params, 0, 1).min(cols.saturating_sub(col));
                lx_info!(
                    "term.edit.ICH",
                    "insert chars",
                    n = n,
                    cur_col = col,
                    cur_row = row,
                    cols = cols
                );
                if n > 0 {
                    // Shift right
                    for c in (col + n..cols).rev() {
                        let src = self.grid.cell(c - n, row);
                        self.grid.set_cell(c, row, src);
                    }
                    let blank = blank_with(*self.attrs);
                    for c in col..col + n {
                        self.grid.set_cell(c, row, blank);
                    }
                }
            }
            b'P' => {
                // DCH: delete N chars at cursor — shift cells
                // [col+n..cols] left to [col..cols-n], blank tail.
                let n = param(params, 0, 1).min(cols.saturating_sub(col));
                lx_info!(
                    "term.edit.DCH",
                    "delete chars",
                    n = n,
                    cur_col = col,
                    cur_row = row,
                    cols = cols
                );
                if n > 0 {
                    for c in col..cols - n {
                        let src = self.grid.cell(c + n, row);
                        self.grid.set_cell(c, row, src);
                    }
                    let blank = blank_with(*self.attrs);
                    for c in cols - n..cols {
                        self.grid.set_cell(c, row, blank);
                    }
                }
            }
            b'X' => {
                // ECH: erase N chars at cursor (don't shift, just blank
                // in place). Cursor unchanged.
                let n = param(params, 0, 1).min(cols.saturating_sub(col));
                let blank = blank_with(*self.attrs);
                for c in col..col + n {
                    self.grid.set_cell(c, row, blank);
                }
            }
            b'S' => {
                // SU: scroll up N lines within scroll region. Cursor
                // unchanged.
                let n = param(params, 0, 1);
                self.region_scroll_up(n);
            }
            b'T' => {
                // SD: scroll down N lines within scroll region. Cursor
                // unchanged.
                let n = param(params, 0, 1);
                self.region_scroll_down(n);
            }
            b'c' if intermediates.is_empty() => {
                // DA — Primary Device Attributes. App is asking "who
                // are you?". We report VT220-class with selective
                // attributes: ?62 = VT220-class, 1 = 132-column mode,
                // 2 = printer port (we lie, harmless), 6 = selective
                // erase, 9 = national replacement charset, 22 = colour
                // text — picks up the same shape xterm reports.
                // Without this response apps stall on capability probe
                // and fall back to degraded rendering paths.
                self.pending_response.extend_from_slice(b"\x1b[?62;1;6;22c");
                self.record_response("DA1");
            }
            b'c' if intermediates == b">" => {
                // DA2 (Secondary DA) — `CSI > 0 c`. App wants firmware
                // version. xterm responds `CSI > 41;330;0 c` (terminal
                // type 41 = VT420, version 330, ROM 0). We mimic.
                self.pending_response.extend_from_slice(b"\x1b[>41;330;0c");
                self.record_response("DA2");
            }
            b'q' if intermediates == b">" => {
                // XTQVERSION — `CSI > 0 q`. App wants the terminal's
                // name+version string. Respond with a DCS reply:
                //   DCS > | marspot ESC \
                // Apps that recognise this fingerprint can tune their
                // behaviour; apps that don't ignore it.
                self.pending_response.extend_from_slice(b"\x1bP>|marspot\x1b\\");
                self.record_response("XTQVERSION");
            }
            _ => {
                // remaining CSI commands arrive in later phases —
                // surface them at DEBUG so we know what apps are
                // sending that we drop on the floor.  Sampled because
                // an unknown sequence in a loop would otherwise flood.
                lx_debug_sampled!(
                    "term.csi.unsupported",
                    16,
                    "unhandled CSI final",
                    final_byte = byte as char,
                    param0 = params.first().copied().unwrap_or(0)
                );
            }
        }
    }

    fn osc_dispatch(&mut self, data: &[u8]) {
        self.flush_cluster_for_break();
        // OSC handlers (window title, hyperlinks, palette) — later phase.
        // Surface what's coming through at DEBUG so the next round of
        // OSC implementation has a "what do apps actually send" log.
        lx_debug!(
            "term.osc.dispatch",
            "OSC payload (handler not implemented yet)",
            bytes = data.len(),
            head = data
                .first()
                .copied()
                .map(|b| b as char)
                .unwrap_or('?')
        );
    }
}

/// Look up a CSI parameter, treating `0` and "missing" both as the supplied
/// default — this matches the standard convention where omitted params and
/// explicit `0` are equivalent for cursor movement and most other CSIs.
fn param(params: &[u16], idx: usize, default: u16) -> u16 {
    match params.get(idx).copied() {
        Some(0) | None => default,
        Some(n) => n,
    }
}

/// Look up a CSI parameter without folding `0` into the default.  ED and EL
/// use this convention: the explicit `0` is a real selector (= "from cursor
/// to end"), distinct from "omitted" which is also `0` here.
fn param_raw(params: &[u16], idx: usize, default: u16) -> u16 {
    params.get(idx).copied().unwrap_or(default)
}

/// A blank ' ' stamped with the supplied attrs.  Used wherever new cells
/// appear (BCE for erase, scroll-fill for the new bottom row).  Apps like
/// vim and tmux rely on this — clearing or scrolling under a non-default
/// background must produce colored cells, not transparent ones.
/// One-shot init: open the trace file path from MARSPOT_TRACE_ESC env.
/// Set to None when env isn't present so the per-byte dispatch path
/// pays only the OnceLock load (~1 ns) when tracing is off.
fn trace_seq(kind: &str, intermediates: &[u8], params: &[u16], byte: u8) {
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};
    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    let file = FILE.get_or_init(|| {
        std::env::var("MARSPOT_TRACE_ESC")
            .ok()
            .and_then(|p| std::fs::OpenOptions::new()
                .create(true).append(true).open(&p)
                .ok()
                .map(Mutex::new))
    });
    let Some(file) = file.as_ref() else { return };
    let mut s = String::with_capacity(64);
    s.push_str(kind);
    if !intermediates.is_empty() {
        s.push(' ');
        for &b in intermediates {
            s.push(b as char);
        }
    }
    for (i, p) in params.iter().enumerate() {
        s.push(if i == 0 { ' ' } else { ';' });
        s.push_str(&p.to_string());
    }
    s.push(' ');
    if (0x20..=0x7e).contains(&byte) {
        s.push(byte as char);
    } else {
        s.push_str(&format!("0x{:02x}", byte));
    }
    s.push('\n');
    if let Ok(mut f) = file.lock() {
        let _ = f.write_all(s.as_bytes());
    }
}

fn blank_with(attrs: CellAttrs) -> Cell {
    Cell { ch: ' ', attrs }
}

fn fill_range(grid: &mut Grid, start: u32, end_exclusive: u32, attrs: CellAttrs) {
    if start >= end_exclusive {
        return;
    }
    let cols = grid.cols() as u32;
    let blank = blank_with(attrs);
    // ─── Wide-pair boundary repair ───────────────────────────────────
    //
    // A wide grapheme (CJK, emoji, our Ambiguous=Wide set: ① ★ ▲ etc.)
    // occupies TWO adjacent grid cells: a `lead` carrying the glyph
    // and a `trail` carrying `\0` as a sentinel.  If `fill_range`
    // straddles a wide pair, leaving only one half intact creates a
    // ghost: the renderer keeps painting the (still-present) lead's
    // wide glyph over the (now-blank) trail slot, or a stale `\0`
    // trail next to a blank lead reads as a phantom space.  This was
    // the 2026-06-15 "横线残留" report — claudecode emitted EL on a
    // row containing a box-drawing wide sequence and a fragment of
    // the old line survived the wipe.
    //
    // Fix both edges:
    //
    //   left:  if `start` is a wide TRAIL (the cell just before on
    //          the same row is a wide lead), pull `start` back by 1
    //          so the orphan lead gets blanked too.
    //
    //   right: if `end-1` is a wide LEAD, its trail sits at `end`.
    //          Extend `end` by 1 so the orphan trail is reset.
    //
    // Both checks skip when the boundary falls on a row break
    // (col == 0) — wide pairs can't straddle rows (the parser breaks
    // them at the last column).
    let mut start = start;
    let mut end_exclusive = end_exclusive;
    if start > 0 && start % cols != 0 {
        let prev_col = ((start - 1) % cols) as u16;
        let row = ((start - 1) / cols) as u16;
        let prev = grid.cell(prev_col, row);
        if crate::grid::char_width(prev.ch) == 2 {
            start -= 1;
        }
    }
    let total = cols * grid.rows() as u32;
    if end_exclusive < total && end_exclusive % cols != 0 {
        let last_col = ((end_exclusive - 1) % cols) as u16;
        let row = ((end_exclusive - 1) / cols) as u16;
        let last = grid.cell(last_col, row);
        if crate::grid::char_width(last.ch) == 2 {
            end_exclusive += 1;
        }
    }
    for idx in start..end_exclusive {
        let col = (idx % cols) as u16;
        let row = (idx / cols) as u16;
        grid.set_cell(col, row, blank);
    }
}

fn erase_in_display(
    grid: &mut Grid,
    col: u16,
    row: u16,
    cols: u16,
    rows: u16,
    mode: u16,
    attrs: CellAttrs,
) {
    let total = cols as u32 * rows as u32;
    let cursor_idx = row as u32 * cols as u32 + col as u32;
    match mode {
        0 => fill_range(grid, cursor_idx, total, attrs),
        1 => fill_range(grid, 0, cursor_idx + 1, attrs),
        2 => fill_range(grid, 0, total, attrs),
        // 3 = erase scrollback — deferred until scrollback exists (1.1.5).
        _ => {}
    }
}

fn erase_in_line(grid: &mut Grid, col: u16, row: u16, cols: u16, mode: u16, attrs: CellAttrs) {
    let row_start = row as u32 * cols as u32;
    let row_end = row_start + cols as u32;
    let cursor_idx = row_start + col as u32;
    match mode {
        0 => fill_range(grid, cursor_idx, row_end, attrs),
        1 => fill_range(grid, row_start, cursor_idx + 1, attrs),
        2 => fill_range(grid, row_start, row_end, attrs),
        _ => {}
    }
}

/// Apply a CSI SGR (Select Graphic Rendition) sequence.  Empty params is
/// equivalent to `[0]` (reset), per the standard.
///
/// We walk the param list with an explicit index because 38/48 (extended
/// color) consume additional params depending on the second value.
fn apply_sgr(attrs: &mut CellAttrs, params: &[u16]) {
    if params.is_empty() {
        *attrs = CellAttrs::default();
        return;
    }
    let mut i = 0;
    while i < params.len() {
        match params[i] {
            0 => *attrs = CellAttrs::default(),
            1 => attrs.bold = true,
            2 => attrs.dim = true,
            3 => attrs.italic = true,
            4 => attrs.underline = true,
            7 => attrs.reverse = true,
            // SGR 22 is "normal intensity" — clears BOTH bold and dim
            // per ECMA-48, not just bold. TUIs (claudecode dim spans)
            // emit `2 ... 22` pairs and expect 22 to fully restore.
            22 => { attrs.bold = false; attrs.dim = false; }
            23 => attrs.italic = false,
            24 => attrs.underline = false,
            27 => attrs.reverse = false,
            // Standard 8-color foreground.
            n @ 30..=37 => attrs.fg = Color::Indexed((n - 30) as u8),
            // Extended foreground: 38;5;n (256-color) or 38;2;r;g;b (RGB).
            38 => {
                if let Some((color, consumed)) = parse_extended_color(&params[i + 1..]) {
                    attrs.fg = color;
                    i += consumed;
                }
            }
            39 => attrs.fg = Color::Default,
            n @ 40..=47 => attrs.bg = Color::Indexed((n - 40) as u8),
            48 => {
                if let Some((color, consumed)) = parse_extended_color(&params[i + 1..]) {
                    attrs.bg = color;
                    i += consumed;
                }
            }
            49 => attrs.bg = Color::Default,
            // Bright foreground (8–15).
            n @ 90..=97 => attrs.fg = Color::Indexed(8 + (n - 90) as u8),
            n @ 100..=107 => attrs.bg = Color::Indexed(8 + (n - 100) as u8),
            _ => {} // unknown / unimplemented SGR code: silently skip
        }
        i += 1;
    }
}

/// Parse the tail of a 38/48 sequence.  Returns the resulting `Color` and
/// the number of *additional* params consumed beyond the 38/48 itself, so
/// the caller can advance its index.
///
/// 5;n           → Indexed(n) — 256-color palette
/// 2;r;g;b       → Rgb(r,g,b) — direct color
/// anything else → None (caller leaves attrs unchanged and advances 1)
fn parse_extended_color(rest: &[u16]) -> Option<(Color, usize)> {
    match rest.first().copied()? {
        5 => {
            let n = rest.get(1).copied()?;
            Some((Color::Indexed(n.min(255) as u8), 2))
        }
        2 => {
            let r = rest.get(1).copied()?;
            let g = rest.get(2).copied()?;
            let b = rest.get(3).copied()?;
            Some((Color::Rgb(r.min(255) as u8, g.min(255) as u8, b.min(255) as u8), 4))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term_with(cols: u16, rows: u16, bytes: &[u8]) -> Terminal {
        let mut t = Terminal::new(cols, rows);
        t.feed(bytes);
        t
    }

    // ─── RFC-002 step 2 snapshot tests ───────────────────────────────

    /// Helper: deep-equal grid cells + cursor + the bits apply_snapshot
    /// actually restores.  Lets snapshot tests assert "after apply, the
    /// terminal is observationally indistinguishable from the source"
    /// without manually comparing every field.
    fn assert_terms_equivalent(a: &Terminal, b: &Terminal) {
        let ga = a.grid();
        let gb = b.grid();
        assert_eq!(ga.cols(), gb.cols(), "cols mismatch");
        assert_eq!(ga.rows(), gb.rows(), "rows mismatch");
        assert_eq!(ga.cursor(), gb.cursor(), "cursor mismatch");
        assert_eq!(a.scroll_top, b.scroll_top, "scroll_top");
        assert_eq!(a.scroll_bot, b.scroll_bot, "scroll_bot");
        assert_eq!(a.cursor_key_application_mode, b.cursor_key_application_mode, "DECCKM");
        assert_eq!(a.bracketed_paste_mode, b.bracketed_paste_mode, "bracketed paste");
        assert_eq!(a.cursor_visible, b.cursor_visible, "cursor visible");
        assert_eq!(a.pending_wrap, b.pending_wrap, "pending_wrap");
        assert_eq!(a.attrs, b.attrs, "current SGR attrs");
        assert_eq!(a.saved_cursor.map(|s| (s.col, s.row, s.attrs)),
                   b.saved_cursor.map(|s| (s.col, s.row, s.attrs)), "saved cursor");
        assert_eq!(a.generation, b.generation, "generation");
        for r in 0..ga.rows() {
            for c in 0..ga.cols() {
                let ca = ga.cell(c, r);
                let cb = gb.cell(c, r);
                assert_eq!(ca, cb, "cell mismatch at ({},{})", c, r);
            }
        }
    }

    #[test]
    fn snapshot_roundtrip_empty_terminal() {
        // Fresh terminal: no feed, generation 0, all defaults.
        // Round-tripping a default state must produce a default state.
        let src = Terminal::new(20, 5);
        let bytes = src.serialize_snapshot();
        let mut dst = Terminal::new(1, 1); // intentionally different dims
        dst.apply_snapshot(&bytes).unwrap();
        assert_terms_equivalent(&src, &dst);
    }

    #[test]
    fn snapshot_roundtrip_with_text_and_cursor() {
        // "Hello\nWorld" + a couple SGR runs: covers ascii cells,
        // cursor advancement, and a newline that scroll-region-aware.
        let src = term_with(20, 5, b"\x1b[31mHello\x1b[0m\r\nWorld");
        let bytes = src.serialize_snapshot();
        let mut dst = Terminal::new(20, 5);
        dst.apply_snapshot(&bytes).unwrap();
        assert_terms_equivalent(&src, &dst);
        // And the wire form is non-trivial — at least the header +
        // 5 rows × 20 cols × 13 = 1300 cells + ~50 bytes header.
        assert!(bytes.len() > 1000, "wire form suspiciously small: {}", bytes.len());
    }

    #[test]
    fn snapshot_roundtrip_with_cjk_wide_cells() {
        // 中文字符占 2 cells — wide pairing is a known fragile area.
        // After roundtrip, the lead cell must still hold the CJK
        // char + the trail must still hold the wide-sentinel.
        let src = term_with(10, 3, "中文测试".as_bytes());
        let bytes = src.serialize_snapshot();
        let mut dst = Terminal::new(10, 3);
        dst.apply_snapshot(&bytes).unwrap();
        assert_terms_equivalent(&src, &dst);
    }

    #[test]
    fn snapshot_roundtrip_preserves_rgb_and_indexed_colors() {
        // SGR 38;2;r;g;b (RGB) + SGR 38;5;n (indexed) + default —
        // exercises all three Color variants in the per-cell attrs.
        let src = term_with(
            30, 3,
            b"\x1b[38;2;200;100;50mRGB\x1b[38;5;82mIDX\x1b[0mDEF",
        );
        let bytes = src.serialize_snapshot();
        let mut dst = Terminal::new(30, 3);
        dst.apply_snapshot(&bytes).unwrap();
        assert_terms_equivalent(&src, &dst);
    }

    #[test]
    fn snapshot_roundtrip_with_saved_cursor_and_modes() {
        // DECSC (ESC 7) saves cursor + attrs; DECSET ?1, ?2004, ?25
        // exercise the mode bitset path.  After roundtrip the
        // SavedCursor option + modes must survive.
        let src = term_with(
            20, 5,
            b"\x1b[31mAB\x1b 7\x1b[?1h\x1b[?2004h\x1b[?25l",
        );
        let bytes = src.serialize_snapshot();
        let mut dst = Terminal::new(20, 5);
        dst.apply_snapshot(&bytes).unwrap();
        assert_terms_equivalent(&src, &dst);
        assert!(dst.cursor_key_application_mode);
        assert!(dst.bracketed_paste_mode);
        assert!(!dst.cursor_visible);
        assert!(dst.saved_cursor.is_some());
    }

    #[test]
    fn snapshot_apply_rejects_bad_magic() {
        let mut t = Terminal::new(20, 5);
        let bad = [0u8; 64]; // all zeros — magic is 0xA557_5301
        let err = t.apply_snapshot(&bad).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn snapshot_apply_rejects_truncated_body() {
        let src = term_with(20, 5, b"hello");
        let bytes = src.serialize_snapshot();
        // Truncate to header-only.
        let truncated = &bytes[..30];
        let mut dst = Terminal::new(20, 5);
        let err = dst.apply_snapshot(truncated).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn feed_bumps_generation_only_on_non_empty_input() {
        let mut t = Terminal::new(20, 5);
        assert_eq!(t.generation(), 0);
        t.feed(b""); // no-op
        assert_eq!(t.generation(), 0, "empty feed must not bump");
        t.feed(b"x");
        assert_eq!(t.generation(), 1);
        t.feed(b"yz");
        assert_eq!(t.generation(), 2, "each non-empty feed bumps by 1");
    }

    #[test]
    fn snapshot_then_more_input_advances_generation_past_loaded_value() {
        // Generation is observational: after apply, additional feed
        // bumps past the loaded value.  Critical for RFC-002 LWW
        // convergence (the client must keep moving forward).
        let src = term_with(20, 5, b"hello");
        let loaded_gen = src.generation();
        assert!(loaded_gen >= 1);
        let bytes = src.serialize_snapshot();
        let mut dst = Terminal::new(20, 5);
        dst.apply_snapshot(&bytes).unwrap();
        assert_eq!(dst.generation(), loaded_gen);
        dst.feed(b"world");
        assert_eq!(dst.generation(), loaded_gen + 1);
    }

    #[test]
    fn scrollback_page_roundtrip_recovers_lines() {
        // 8-col / 2-row grid + \r\n between single-char lines avoids
        // wrap weirdness: each line is one cell + spaces, scroll pushes
        // the top row into scrollback whole.  Feed A..F (6 lines) →
        // grid keeps the last two (E, F), scrollback holds A,B,C,D.
        let mut t = Terminal::new(8, 2);
        t.feed(b"\x1b[31mA\r\nB\r\nC\r\nD\r\nE\r\nF");
        let total = t.grid().scrollback_len();
        assert_eq!(total, 4);
        let (line_count, body) = t.serialize_scrollback_page(0, total as u32);
        assert_eq!(line_count, 4);
        let lines = Terminal::decode_scrollback_page_body(line_count, &body).unwrap();
        assert_eq!(lines.len(), 4);
        // Read_lines is oldest-first: A,B,C,D.
        assert_eq!(lines[0][0].ch, 'A');
        assert_eq!(lines[3][0].ch, 'D');
        // Foreground colour survives the wire format.
        assert!(matches!(lines[0][0].attrs.fg, Color::Indexed(1)));
        // Each line padded to grid width.
        assert_eq!(lines[0].len(), 8);
    }

    #[test]
    fn scrollback_page_count_overrun_clamps_to_available() {
        let mut t = Terminal::new(8, 2);
        t.feed(b"A\r\nB\r\nC\r\nD\r\nE\r\nF");
        let (line_count, body) = t.serialize_scrollback_page(0, 100);
        assert_eq!(line_count, 4);
        let lines = Terminal::decode_scrollback_page_body(line_count, &body).unwrap();
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn push_historic_line_fills_scrollback_in_caller_order() {
        // Empty scrollback → push 3 historic lines oldest-first →
        // scrollback_line at idx 0..3 returns them in the same order,
        // and a subsequent live-data scroll-up appends after them.
        let mut t = Terminal::new(8, 2);
        let attrs = CellAttrs::default();
        let mkline = |ch: char| -> Vec<Cell> {
            (0..8).map(|_| Cell { ch, attrs }).collect()
        };
        t.push_historic_line(&mkline('H'));
        t.push_historic_line(&mkline('I'));
        t.push_historic_line(&mkline('J'));
        assert_eq!(t.grid().scrollback_len(), 3);
        assert_eq!(t.grid().scrollback_line(0).unwrap()[0].ch, 'H');
        assert_eq!(t.grid().scrollback_line(2).unwrap()[0].ch, 'J');
        // Live data continues from where historic ended.
        t.feed(b"K\r\nL\r\nM\r\nN");
        // 4 new lines, grid rows=2, so 2 of them spill into scrollback
        // (oldest-first append) → scrollback now [H, I, J, K, L].
        assert_eq!(t.grid().scrollback_len(), 5);
        assert_eq!(t.grid().scrollback_line(3).unwrap()[0].ch, 'K');
        assert_eq!(t.grid().scrollback_line(4).unwrap()[0].ch, 'L');
    }

    #[test]
    fn scrollback_page_empty_scrollback_returns_zero_count() {
        let t = Terminal::new(80, 24);
        let (line_count, body) = t.serialize_scrollback_page(0, 50);
        assert_eq!(line_count, 0);
        assert!(body.is_empty());
        let lines = Terminal::decode_scrollback_page_body(line_count, &body).unwrap();
        assert!(lines.is_empty());
    }

    #[test]
    fn snapshot_size_is_proportional_to_grid_area() {
        // 80×24 grid ≈ 80*24*13 = 24960 + ~50 header.
        let src = Terminal::new(80, 24);
        let bytes = src.serialize_snapshot();
        let cells_part = 80 * 24 * CELL_BYTES;
        assert!(
            bytes.len() >= cells_part && bytes.len() <= cells_part + 256,
            "wire size {} not within [cells={}, +256]",
            bytes.len(),
            cells_part
        );
    }

    #[test]
    fn plain_ascii_writes_cells_and_advances_cursor() {
        let t = term_with(80, 24, b"hi");
        assert_eq!(t.grid().cell(0, 0).ch, 'h');
        assert_eq!(t.grid().cell(1, 0).ch, 'i');
        assert_eq!(t.grid().cursor(), (2, 0));
    }

    #[test]
    fn esc_7_8_save_and_restore_cursor() {
        // DECSC (ESC 7) saves, DECRC (ESC 8) restores. The cursor at
        // save time gets stamped back even after intervening movement.
        let t = term_with(20, 5, b"\x1b[3;6H\x1b7\x1b[1;1H\x1b8");
        // CUP 3;6 = row 3 col 6 (1-indexed) → (5, 2) 0-indexed.
        assert_eq!(t.grid().cursor(), (5, 2));
    }

    #[test]
    fn csi_s_u_save_and_restore_cursor() {
        // SCO variant — CSI s / CSI u — must behave identically to
        // ESC 7 / ESC 8 for the cursor.
        let t = term_with(20, 5, b"\x1b[2;3H\x1b[s\x1b[5;5H\x1b[u");
        // Should be back at (col=2, row=1).
        assert_eq!(t.grid().cursor(), (2, 1));
    }

    #[test]
    fn restore_without_prior_save_is_a_noop() {
        // xterm semantics: CSI u / ESC 8 with no save = no-op, cursor
        // stays where it is. (Some terminals reset to home; xterm doesn't.)
        let t = term_with(20, 5, b"\x1b[2;3H\x1b[u\x1b8");
        assert_eq!(t.grid().cursor(), (2, 1));
    }

    #[test]
    fn decstbm_sets_scroll_region_and_homes_cursor() {
        // CSI 3;7 r — region rows 3..=7 (1-indexed → 2..=6 0-indexed).
        // Cursor should move to (0, 0) per xterm.
        let t = term_with(20, 10, b"\x1b[5;5H\x1b[3;7r");
        assert_eq!(t.grid().cursor(), (0, 0));
        // Verify the region took effect via LF behaviour: cursor at
        // row 6 (0-indexed) is the scroll_bot — next LF scrolls
        // within the region, not the whole grid.
    }

    #[test]
    fn lf_at_scroll_bot_scrolls_region_only() {
        // Set region 0..=2 (rows 1..=3 1-indexed). Fill rows 0, 1, 2,
        // 3 with distinctive markers; verify after LF at row 2 the
        // row 3 content is intact (didn't scroll with region).
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1b[1;3r");          // region [0..=2]
        t.feed(b"\x1b[1;1HAA\r\n");   // row 0 "AA"
        t.feed(b"BB\r\n");             // row 1 "BB"
        t.feed(b"CC\r\n");             // row 2 "CC", then LF triggers region scroll
        t.feed(b"DD");                 // ???
        // After the LF after writing "CC", cursor was at row 2 (scroll_bot),
        // region scrolls up. Row 0 "AA" pushed off the region (NOT into
        // scrollback because region != full grid). Row 1 "BB" → row 0,
        // row 2 "CC" → row 1, row 2 blanked. Then "DD" written at row 2.
        assert_eq!(t.grid().cell(0, 0).ch, 'B', "row 0 should be BB after scroll");
        assert_eq!(t.grid().cell(0, 1).ch, 'C', "row 1 should be CC after scroll");
        assert_eq!(t.grid().cell(0, 2).ch, 'D', "row 2 should be DD (new content)");
        // Row 3 (outside region) should be unchanged from initial (blank).
        assert_eq!(t.grid().cell(0, 3).ch, ' ', "row 3 is outside region, unchanged");
    }

    #[test]
    fn il_inserts_blank_lines_at_cursor() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1b[1;1HAA\r\n");   // row 0
        t.feed(b"BB\r\n");             // row 1
        t.feed(b"CC\r\n");             // row 2
        t.feed(b"\x1b[2;1H");          // cursor to row 1
        t.feed(b"\x1b[L");             // IL: insert 1 line at row 1
        // Row 0 unchanged. Row 1 blank. Row 2 used to be BB → now is BB
        // (shifted down). Row 3 used to be CC → now CC.
        assert_eq!(t.grid().cell(0, 0).ch, 'A');
        assert_eq!(t.grid().cell(0, 1).ch, ' ', "inserted blank");
        assert_eq!(t.grid().cell(0, 2).ch, 'B');
        assert_eq!(t.grid().cell(0, 3).ch, 'C');
    }

    #[test]
    fn dl_deletes_lines_at_cursor() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"AA\r\nBB\r\nCC\r\nDD\r\n");
        t.feed(b"\x1b[2;1H");          // cursor to row 1
        t.feed(b"\x1b[M");             // DL: delete 1 line at row 1
        assert_eq!(t.grid().cell(0, 0).ch, 'A');
        assert_eq!(t.grid().cell(0, 1).ch, 'C', "row 2 (CC) shifted up");
        assert_eq!(t.grid().cell(0, 2).ch, 'D', "row 3 (DD) shifted up");
        assert_eq!(t.grid().cell(0, 3).ch, ' ', "row 3 blanked");
    }

    #[test]
    fn da_csi_c_queues_xterm_compatible_response() {
        // CSI c — Primary Device Attributes. App is asking "what
        // terminal class are you?". We respond with a VT220-compatible
        // attr string so apps that probe capabilities at startup get
        // an answer and don't fall back to degraded rendering.
        let mut t = Terminal::new(20, 5);
        t.feed(b"\x1b[c");
        let resp = t.take_response();
        assert_eq!(resp, b"\x1b[?62;1;6;22c");
        // Second take should return empty (state was consumed).
        let resp2 = t.take_response();
        assert!(resp2.is_empty());
    }

    #[test]
    fn el_when_cursor_on_wide_trail_clears_lead() {
        // 2026-06-15 "横线残留" repro.  Wide glyph at col 0-1 (中
        // as lead, '\0' trail — guaranteed wide via the EAW table,
        // independent of MARSPOT_AMBIGUOUS_WIDE).  CUP then
        // positions cursor onto col 1 (the trail slot) — happens
        // when an app's wcwidth disagrees with marspot's stored
        // width for that codepoint.  EL mode=0 then fills [1..10)
        // — without boundary repair the lead at col 0 stays intact,
        // rendering as a wide glyph spilling into the now-blank
        // trail.  Boundary repair pulls start back to col 0.
        let mut t = Terminal::new(10, 3);
        t.feed("中".as_bytes());
        t.feed(b"\x1b[1;2H");
        assert_eq!(t.grid().cursor(), (1, 0));
        t.feed(b"\x1b[K");
        assert_eq!(t.grid().cell(0, 0).ch, ' ', "wide-lead orphan not cleared");
        assert_eq!(t.grid().cell(1, 0).ch, ' ', "wide-trail not cleared");
    }

    #[test]
    fn ed_partial_into_wide_lead_clears_trail() {
        // Mirror case for the right edge of fill_range.  Wide glyph
        // at col 4-5 (中 — unconditionally wide), cursor at col 6.
        // CUP to col 4 then EL mode=1 (erase from begin to cursor
        // inclusive).  Cursor cell is the wide LEAD at col 4.
        let mut t = Terminal::new(10, 3);
        t.feed(b"    ");
        t.feed("中".as_bytes());
        t.feed(b"\x1b[1;5H");
        t.feed(b"\x1b[1K");
        assert_eq!(t.grid().cell(4, 0).ch, ' ', "wide-lead under cursor not cleared");
        assert_eq!(t.grid().cell(5, 0).ch, ' ', "wide-trail orphan not cleared");
    }

    #[test]
    fn da1_burst_tracking_records_per_response() {
        // Five DA1 queries inside the parser's single feed call should
        // surface five responses in pending_response (concatenated)
        // and tick the burst window to five entries.  Exercises the
        // record_response bookkeeping without depending on log sink
        // state — the visible assert is just that the response bytes
        // were queued the expected number of times.
        let mut t = Terminal::new(20, 5);
        // Five consecutive DA1 queries.  Each is a complete escape so
        // the parser dispatches independently.
        t.feed(b"\x1b[c\x1b[c\x1b[c\x1b[c\x1b[c");
        let resp = t.take_response();
        // Each response is `\e[?62;1;6;22c` = 13 bytes.  5 × 13 = 65.
        assert_eq!(resp.len(), 65, "expected 5 concatenated DA1 responses");
        // Sanity-check the start + end mark are present.
        assert!(resp.starts_with(b"\x1b[?62;1;6;22c"));
        assert!(resp.ends_with(b"\x1b[?62;1;6;22c"));
    }

    #[test]
    fn deccm_cursor_key_app_mode_toggle() {
        // CSI ?1 h sets DECCKM (cursor key application mode); CSI ?1 l
        // clears it. The input layer reads this to decide arrow-key
        // encoding (ESC O X vs ESC [ X).
        let mut t = Terminal::new(20, 5);
        assert!(!t.cursor_key_application_mode());
        t.feed(b"\x1b[?1h");
        assert!(t.cursor_key_application_mode());
        t.feed(b"\x1b[?1l");
        assert!(!t.cursor_key_application_mode());
    }

    #[test]
    fn ich_dch_ech_chars() {
        let mut t = Terminal::new(10, 3);
        t.feed(b"ABCDEFG");
        // ICH 2 at col 2 — insert 2 blanks at col 2.
        t.feed(b"\x1b[1;3H");          // cursor (col=2, row=0)
        t.feed(b"\x1b[2@");
        // Was "ABCDEFG"; after ICH 2 at col 2: "AB  CDEF" (G falls off rhs)
        assert_eq!(t.grid().cell(0, 0).ch, 'A');
        assert_eq!(t.grid().cell(1, 0).ch, 'B');
        assert_eq!(t.grid().cell(2, 0).ch, ' ');
        assert_eq!(t.grid().cell(3, 0).ch, ' ');
        assert_eq!(t.grid().cell(4, 0).ch, 'C');

        // DCH 2 at col 2 — delete the 2 blanks; "ABCDEF" comes back.
        t.feed(b"\x1b[1;3H\x1b[2P");
        assert_eq!(t.grid().cell(0, 0).ch, 'A');
        assert_eq!(t.grid().cell(1, 0).ch, 'B');
        assert_eq!(t.grid().cell(2, 0).ch, 'C');
        assert_eq!(t.grid().cell(3, 0).ch, 'D');

        // ECH 2 at col 0 — erase chars in place, no shift.
        t.feed(b"\x1b[1;1H\x1b[2X");
        assert_eq!(t.grid().cell(0, 0).ch, ' ');
        assert_eq!(t.grid().cell(1, 0).ch, ' ');
        assert_eq!(t.grid().cell(2, 0).ch, 'C', "no shift");
    }

    #[test]
    fn save_restore_round_trips_sgr_attrs() {
        // The save snapshot includes SGR — restoring brings back the
        // attrs at save time, even after later SGR changes. Verified
        // by colouring a glyph after restore and reading the cell back.
        let mut t = Terminal::new(20, 5);
        t.feed(b"\x1b[31m");      // red foreground
        t.feed(b"\x1b[2;2H");     // cursor (1, 1)
        t.feed(b"\x1b7");         // save (cursor + attrs)
        t.feed(b"\x1b[34m");      // change to blue
        t.feed(b"\x1b[5;5HX");    // write X in blue at (4, 4)
        t.feed(b"\x1b8");         // restore (cursor → (1,1), attrs → red)
        t.feed(b"Y");             // write Y at (1, 1) in red
        let y = t.grid().cell(1, 1);
        let x = t.grid().cell(4, 4);
        assert_eq!(y.ch, 'Y');
        assert_eq!(x.ch, 'X');
        // The exact Color repr matters less than that they differ —
        // both should be palette colours, with Y in red (3, 31) and
        // X in blue (4, 34). The SGR restore should make y.attrs.fg !=
        // x.attrs.fg.
        assert_ne!(y.attrs.fg, x.attrs.fg);
    }

    #[test]
    fn lf_only_advances_row_does_not_reset_column() {
        // xterm default (LNM unset): LF moves down only.  CR is needed to
        // return to column 0.  Many real shells emit CRLF together.
        let t = term_with(80, 24, b"abc\n");
        assert_eq!(t.grid().cursor(), (3, 1));
    }

    #[test]
    fn crlf_returns_to_next_line_start() {
        let t = term_with(80, 24, b"abc\r\n");
        assert_eq!(t.grid().cursor(), (0, 1));
    }

    #[test]
    fn csi_a_moves_cursor_up() {
        let t = term_with(80, 24, b"\n\n\n\x1B[2A");
        // After 3 LFs cursor is at row 3, col 0.  CUU 2 → row 1.
        assert_eq!(t.grid().cursor(), (0, 1));
    }

    #[test]
    fn csi_b_moves_cursor_down() {
        let t = term_with(80, 24, b"\x1B[3B");
        assert_eq!(t.grid().cursor(), (0, 3));
    }

    #[test]
    fn csi_c_moves_cursor_right() {
        let t = term_with(80, 24, b"\x1B[4C");
        assert_eq!(t.grid().cursor(), (4, 0));
    }

    #[test]
    fn csi_d_moves_cursor_left() {
        let t = term_with(80, 24, b"abcdef\x1B[3D");
        assert_eq!(t.grid().cursor(), (3, 0));
    }

    #[test]
    fn csi_no_param_moves_by_one() {
        let t = term_with(80, 24, b"\x1B[B");
        assert_eq!(t.grid().cursor(), (0, 1));
    }

    #[test]
    fn csi_zero_param_treated_as_default() {
        // Param `0` and omitted are both "default" → 1 for cursor moves.
        let t = term_with(80, 24, b"\x1B[0B");
        assert_eq!(t.grid().cursor(), (0, 1));
    }

    #[test]
    fn csi_h_no_params_homes_cursor() {
        let t = term_with(80, 24, b"abc\n\n\x1B[H");
        assert_eq!(t.grid().cursor(), (0, 0));
    }

    #[test]
    fn csi_h_with_row_and_col_positions_cursor() {
        // CSI 3 ; 5 H → row 3, col 5 (1-indexed) → (col=4, row=2) 0-indexed.
        let t = term_with(80, 24, b"\x1B[3;5H");
        assert_eq!(t.grid().cursor(), (4, 2));
    }

    #[test]
    fn csi_f_alias_for_cup() {
        let t = term_with(80, 24, b"\x1B[3;5f");
        assert_eq!(t.grid().cursor(), (4, 2));
    }

    #[test]
    fn csi_g_sets_horizontal_position_absolute() {
        let t = term_with(80, 24, b"\n\x1B[10G");
        // VPA preserved at row 1; col = 10 (1-indexed) = 9 (0-indexed).
        assert_eq!(t.grid().cursor(), (9, 1));
    }

    #[test]
    fn csi_d_lowercase_sets_vertical_position_absolute() {
        let t = term_with(80, 24, b"abc\x1B[5d");
        // HPA preserved at col 3; row = 5 (1-indexed) = 4 (0-indexed).
        assert_eq!(t.grid().cursor(), (3, 4));
    }

    #[test]
    fn cursor_clamps_at_top_left() {
        let t = term_with(80, 24, b"\x1B[A\x1B[D");
        // CUU and CUB at origin must not underflow; cursor stays at (0,0).
        assert_eq!(t.grid().cursor(), (0, 0));
    }

    #[test]
    fn cursor_clamps_at_bottom_right() {
        // CUP off-grid: 99;99 gets clamped to (rows-1, cols-1).  Then CUF /
        // CUD must not move further.
        let t = term_with(80, 24, b"\x1B[99;99H\x1B[10C\x1B[10B");
        assert_eq!(t.grid().cursor(), (79, 23));
    }

    #[test]
    fn print_after_cursor_move_writes_at_new_position() {
        // Move cursor then print: cell at the moved-to position must hold
        // the printed glyph, not the original origin.
        let t = term_with(80, 24, b"\x1B[5;3HX");
        assert_eq!(t.grid().cell(2, 4).ch, 'X');
        assert_eq!(t.grid().cursor(), (3, 4));
    }

    /// Fill the grid with a marker char at every cell so erase regions are
    /// visible by absence.  Returns a Terminal with cursor parked at the
    /// requested (col, row) and all cells populated.
    fn filled_terminal(cols: u16, rows: u16, marker: char, cur_col: u16, cur_row: u16) -> Terminal {
        let mut t = Terminal::new(cols, rows);
        // Manually fill: write `marker` cols times per row, no parsing.
        // We use direct grid access only in tests — production code goes
        // through Terminal::feed.
        for r in 0..rows {
            for c in 0..cols {
                t.grid.set_cell(c, r, Cell { ch: marker, ..Default::default() });
            }
        }
        t.grid.set_cursor(cur_col, cur_row);
        t
    }

    fn count_marker(t: &Terminal, marker: char) -> usize {
        let g = t.grid();
        let mut n = 0;
        for r in 0..g.rows() {
            for c in 0..g.cols() {
                if g.cell(c, r).ch == marker {
                    n += 1;
                }
            }
        }
        n
    }

    #[test]
    fn ed_zero_erases_from_cursor_to_end_of_screen() {
        let mut t = filled_terminal(10, 5, '#', 3, 2);
        let before = count_marker(&t, '#'); // 50
        t.feed(b"\x1B[J"); // ED default = 0
        let cleared = before - count_marker(&t, '#');
        // From (3, 2) inclusive to end of screen: row 2 has cols 3..10 = 7,
        // rows 3 and 4 each have 10, total = 7 + 20 = 27.
        assert_eq!(cleared, 27);
        // Cursor unchanged.
        assert_eq!(t.grid().cursor(), (3, 2));
        // Cells before cursor untouched.
        assert_eq!(t.grid().cell(2, 2).ch, '#');
        assert_eq!(t.grid().cell(3, 2).ch, ' ');
        assert_eq!(t.grid().cell(0, 4).ch, ' ');
    }

    #[test]
    fn ed_one_erases_from_start_to_cursor_inclusive() {
        let mut t = filled_terminal(10, 5, '#', 3, 2);
        t.feed(b"\x1B[1J");
        let remaining = count_marker(&t, '#');
        // Row 2 cols 0..=3 cleared (4 cells), rows 0 and 1 cleared (20).
        // Total cleared: 24.  Remaining: 50 - 24 = 26.
        assert_eq!(remaining, 26);
        assert_eq!(t.grid().cursor(), (3, 2));
        assert_eq!(t.grid().cell(3, 2).ch, ' ', "cursor cell must be cleared");
        assert_eq!(t.grid().cell(4, 2).ch, '#', "cell after cursor must remain");
    }

    #[test]
    fn ed_two_erases_entire_screen() {
        let mut t = filled_terminal(10, 5, '#', 3, 2);
        t.feed(b"\x1B[2J");
        assert_eq!(count_marker(&t, '#'), 0);
        assert_eq!(t.grid().cursor(), (3, 2), "cursor must not move");
    }

    #[test]
    fn el_zero_erases_from_cursor_to_end_of_line() {
        let mut t = filled_terminal(10, 5, '#', 3, 2);
        t.feed(b"\x1B[K");
        // Row 2 cols 3..10 = 7 cleared.
        assert_eq!(count_marker(&t, '#'), 50 - 7);
        assert_eq!(t.grid().cursor(), (3, 2));
        assert_eq!(t.grid().cell(2, 2).ch, '#');
        assert_eq!(t.grid().cell(9, 2).ch, ' ');
        // Other rows untouched.
        assert_eq!(t.grid().cell(5, 1).ch, '#');
        assert_eq!(t.grid().cell(5, 3).ch, '#');
    }

    #[test]
    fn el_one_erases_from_start_of_line_to_cursor_inclusive() {
        let mut t = filled_terminal(10, 5, '#', 3, 2);
        t.feed(b"\x1B[1K");
        // Row 2 cols 0..=3 = 4 cleared.
        assert_eq!(count_marker(&t, '#'), 50 - 4);
        assert_eq!(t.grid().cell(3, 2).ch, ' ');
        assert_eq!(t.grid().cell(4, 2).ch, '#');
        // Other rows untouched.
        assert_eq!(t.grid().cell(0, 1).ch, '#');
    }

    #[test]
    fn el_two_erases_entire_line() {
        let mut t = filled_terminal(10, 5, '#', 3, 2);
        t.feed(b"\x1B[2K");
        // Row 2 fully cleared (10 cells).
        assert_eq!(count_marker(&t, '#'), 50 - 10);
        assert_eq!(t.grid().cursor(), (3, 2));
        // Other rows untouched.
        assert_eq!(t.grid().cell(0, 1).ch, '#');
        assert_eq!(t.grid().cell(0, 3).ch, '#');
    }

    #[test]
    fn ed_three_does_not_panic_or_modify_screen() {
        // ED 3 erases scrollback in xterm; we have no scrollback yet, so it
        // must be a no-op (not panic, not clear visible screen).
        let mut t = filled_terminal(10, 5, '#', 3, 2);
        t.feed(b"\x1B[3J");
        assert_eq!(count_marker(&t, '#'), 50);
    }

    // ----- SGR (Select Graphic Rendition) -----

    #[test]
    fn sgr_empty_is_full_reset() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[1;31;42m");
        assert!(t.current_attrs().bold);
        assert_eq!(t.current_attrs().fg, Color::Indexed(1));
        t.feed(b"\x1B[m");
        assert_eq!(t.current_attrs(), CellAttrs::default());
    }

    #[test]
    fn sgr_zero_resets_all() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[1;31;42m");
        t.feed(b"\x1B[0m");
        assert_eq!(t.current_attrs(), CellAttrs::default());
    }

    #[test]
    fn sgr_bold_on_off_independent_of_color() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[31m\x1B[1m");
        assert!(t.current_attrs().bold);
        assert_eq!(t.current_attrs().fg, Color::Indexed(1));
        t.feed(b"\x1B[22m"); // un-bold; fg stays
        assert!(!t.current_attrs().bold);
        assert_eq!(t.current_attrs().fg, Color::Indexed(1));
    }

    #[test]
    fn sgr_individual_attribute_toggles() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[3m\x1B[4m\x1B[7m");
        let a = t.current_attrs();
        assert!(a.italic && a.underline && a.reverse);
        t.feed(b"\x1B[23m\x1B[24m\x1B[27m");
        let a = t.current_attrs();
        assert!(!a.italic && !a.underline && !a.reverse);
    }

    #[test]
    fn sgr_standard_8_color_fg() {
        for (code, idx) in (30u16..=37).zip(0u8..=7) {
            let mut t = Terminal::new(10, 5);
            t.feed(format!("\x1B[{}m", code).as_bytes());
            assert_eq!(t.current_attrs().fg, Color::Indexed(idx), "code {} -> idx {}", code, idx);
        }
    }

    #[test]
    fn sgr_standard_8_color_bg() {
        for (code, idx) in (40u16..=47).zip(0u8..=7) {
            let mut t = Terminal::new(10, 5);
            t.feed(format!("\x1B[{}m", code).as_bytes());
            assert_eq!(t.current_attrs().bg, Color::Indexed(idx), "code {} -> idx {}", code, idx);
        }
    }

    #[test]
    fn sgr_bright_8_color() {
        // 90–97 → indices 8–15 (bright fg); 100–107 → bright bg.
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[91m"); // bright red fg
        assert_eq!(t.current_attrs().fg, Color::Indexed(9));
        t.feed(b"\x1B[105m"); // bright magenta bg
        assert_eq!(t.current_attrs().bg, Color::Indexed(13));
    }

    #[test]
    fn sgr_default_fg_and_bg() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[31;41m\x1B[39;49m");
        assert_eq!(t.current_attrs().fg, Color::Default);
        assert_eq!(t.current_attrs().bg, Color::Default);
    }

    #[test]
    fn sgr_256_color_fg() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[38;5;208m");
        assert_eq!(t.current_attrs().fg, Color::Indexed(208));
    }

    #[test]
    fn sgr_256_color_bg() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[48;5;42m");
        assert_eq!(t.current_attrs().bg, Color::Indexed(42));
    }

    #[test]
    fn sgr_truecolor_fg() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[38;2;100;200;50m");
        assert_eq!(t.current_attrs().fg, Color::Rgb(100, 200, 50));
    }

    #[test]
    fn sgr_truecolor_bg() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[48;2;10;20;30m");
        assert_eq!(t.current_attrs().bg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn sgr_combined_in_single_sequence() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[1;31;42m");
        let a = t.current_attrs();
        assert!(a.bold);
        assert_eq!(a.fg, Color::Indexed(1));
        assert_eq!(a.bg, Color::Indexed(2));
    }

    #[test]
    fn sgr_unknown_code_is_skipped_not_panicked() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[1;999;31m"); // 999 unknown
        let a = t.current_attrs();
        assert!(a.bold);
        assert_eq!(a.fg, Color::Indexed(1)); // 31 still applied after 999 skipped
    }

    #[test]
    fn printed_cell_carries_current_attrs() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1B[1;31mA");
        let cell = t.grid().cell(0, 0);
        assert_eq!(cell.ch, 'A');
        assert!(cell.attrs.bold);
        assert_eq!(cell.attrs.fg, Color::Indexed(1));
    }

    #[test]
    fn already_printed_cells_unaffected_by_later_sgr() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"A\x1B[31mB");
        // 'A' was printed before SGR — must keep default attrs.
        assert_eq!(t.grid().cell(0, 0).attrs.fg, Color::Default);
        // 'B' was printed after — must have red fg.
        assert_eq!(t.grid().cell(1, 0).attrs.fg, Color::Indexed(1));
    }

    #[test]
    fn bce_erase_fills_with_current_attrs() {
        // Background Color Erase: ED/EL fills cleared cells with the
        // current SGR attrs, not default.  Critical for vim/tmux which
        // paint full-screen backgrounds via CSI 41 m + CSI 2 J.
        let mut t = Terminal::new(10, 3);
        t.feed(b"\x1B[41m\x1B[2J"); // bg red + clear screen
        for r in 0..3 {
            for c in 0..10 {
                let cell = t.grid().cell(c, r);
                assert_eq!(cell.ch, ' ');
                assert_eq!(cell.attrs.bg, Color::Indexed(1), "cell ({},{}) bg", c, r);
            }
        }
    }

    #[test]
    fn bce_erase_in_line_uses_current_bg() {
        let mut t = Terminal::new(10, 3);
        t.feed(b"\x1B[42m\x1B[K"); // bg green + EL 0
        for c in 0..10 {
            assert_eq!(t.grid().cell(c, 0).attrs.bg, Color::Indexed(2));
        }
        // Other rows untouched.
        assert_eq!(t.grid().cell(0, 1).attrs.bg, Color::Default);
    }

    // ----- print path: wrap and BS -----

    #[test]
    fn print_at_eol_wraps_to_next_row() {
        let mut t = Terminal::new(5, 3);
        t.feed(b"abcdef"); // 5 chars fill row 0; 'f' wraps to row 1 col 0
        assert_eq!(t.grid().cell(0, 0).ch, 'a');
        assert_eq!(t.grid().cell(4, 0).ch, 'e');
        assert_eq!(t.grid().cell(0, 1).ch, 'f');
        assert_eq!(t.grid().cursor(), (1, 1));
    }

    #[test]
    fn bs_decrements_column_and_does_not_erase() {
        let mut t = Terminal::new(10, 3);
        t.feed(b"ab\x08"); // print ab, then BS
        assert_eq!(t.grid().cursor(), (1, 0));
        // BS is non-destructive — 'b' must remain.
        assert_eq!(t.grid().cell(1, 0).ch, 'b');
    }

    #[test]
    fn bs_at_column_zero_is_clamped() {
        let mut t = Terminal::new(10, 3);
        t.feed(b"\x08");
        assert_eq!(t.grid().cursor(), (0, 0));
        t.feed(b"\n\x08"); // LF then BS — col stays 0, row stays 1
        assert_eq!(t.grid().cursor(), (0, 1));
    }

    // ----- scrolling -----

    #[test]
    fn lf_at_bottom_row_scrolls_up_and_pushes_to_scrollback() {
        let mut t = Terminal::new(3, 2);
        t.feed(b"abc"); // row 0 full; DECAWM defers the wrap → cursor parks at last col.
        assert_eq!(t.grid().cursor(), (2, 0));
        // Manually park cursor at last row, last col, then LF.
        t.feed(b"\x1B[2;3H"); // CUP row 2 col 3 (1-indexed) → (col 2, row 1)
        t.feed(b"\n");
        // Row 0 ("abc") goes to scrollback, row 1 becomes blank.
        assert_eq!(t.grid().scrollback_len(), 1);
        let sb = t.grid().scrollback_line(0).unwrap();
        assert_eq!(sb.iter().map(|c| c.ch).collect::<String>(), "abc");
        // Cursor stays on the (now blank) last row.
        assert_eq!(t.grid().cursor(), (2, 1));
        for c in 0..3 {
            assert_eq!(t.grid().cell(c, 1), Cell::default());
        }
    }

    #[test]
    fn print_overflow_at_bottom_row_scrolls() {
        // Print enough to fill the entire 2-row grid; the next print must
        // trigger a scroll, not clamp.
        let mut t = Terminal::new(3, 2);
        t.feed(b"abcdefg"); // 6 chars fill the grid; 'g' triggers scroll
        // After scroll: scrollback contains "abc", visible row 0 = "def",
        // 'g' lands at (0, 1).
        assert_eq!(t.grid().scrollback_len(), 1);
        let sb = t.grid().scrollback_line(0).unwrap();
        assert_eq!(sb.iter().map(|c| c.ch).collect::<String>(), "abc");
        assert_eq!(t.grid().cell(0, 0).ch, 'd');
        assert_eq!(t.grid().cell(2, 0).ch, 'f');
        assert_eq!(t.grid().cell(0, 1).ch, 'g');
        assert_eq!(t.grid().cursor(), (1, 1));
    }

    #[test]
    fn scroll_inherits_current_bg_for_blank_row() {
        // BCE for scroll-fill: when SGR has bg=red and we scroll, the new
        // blank row at the bottom must carry that bg.
        let mut t = Terminal::new(3, 2);
        t.feed(b"\x1B[41m"); // bg red
        t.feed(b"abc\x1B[2;3H\n"); // park cursor at last row, LF → scroll
        for c in 0..3 {
            let cell = t.grid().cell(c, 1);
            assert_eq!(cell.ch, ' ');
            assert_eq!(cell.attrs.bg, Color::Indexed(1));
        }
    }

    #[test]
    fn csi_3_J_clears_scrollback_only() {
        let mut t = Terminal::new(3, 2);
        // Build some scrollback by feeding many lines.
        for _ in 0..5 {
            t.feed(b"xxx\x1B[2;3H\n");
        }
        assert!(t.grid().scrollback_len() > 0);
        // ESC[3J clears scrollback; visible region untouched.
        t.feed(b"\x1B[3J");
        assert_eq!(t.grid().scrollback_len(), 0);
        // Visible row 0 should still hold what was there.
        assert_eq!(t.grid().cell(0, 0).ch, 'x');
    }

    // ----- soak: long-running scroll must not grow memory -----

    /// Read this process's resident set size in bytes.  Used by the soak
    /// test to assert that millions of scrolled lines don't grow the
    /// process — the scrollback ring is pre-allocated and bounded.
    fn current_rss_bytes() -> u64 {
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let r = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDTASKINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                std::mem::size_of::<libc::proc_taskinfo>() as i32,
            )
        };
        assert!(r > 0, "proc_pidinfo failed");
        info.pti_resident_size
    }

    #[test]
    #[ignore = "soak; run via bin/soak.sh"]
    fn soak_scrollback_bounded_under_ten_million_lines() {
        let mut t = Terminal::new(80, 24);
        // Warm up enough to fully wrap the ring once so all anon-mmap
        // pages have been faulted in before we baseline.  Default
        // Terminal uses the disk variant (DISK_SCROLLBACK_RAM_LINES +
        // DISK_SCROLLBACK_PAGES × LINES_PER_PAGE = 26 624 slots);
        // 40 000 warmup lines covers that with ~1.5x margin and
        // also fully wraps the 10 000-slot Memory variant.  Without
        // this the test's "baseline" lands mid-fill and the next
        // burst's lazy faults look like a leak.
        let warmup = b"\x1B[24;80H\n".repeat(40_000);
        t.feed(&warmup);

        let baseline = current_rss_bytes();

        // 10 M lines = ~7.8x the previous 1 M target.  At default
        // scrollback capacity (10 K lines) we wrap the ring ~1000
        // times — far past the point any per-write leak would
        // accumulate measurably.
        let line = b"this is a fairly typical 50-character log line!\n";
        let park = b"\x1B[24;80H";
        for _ in 0..10_000_000 {
            t.feed(line);
            // After each line, park cursor at last row so the next \n scrolls.
            t.feed(park);
        }

        let after = current_rss_bytes();
        let growth = after.saturating_sub(baseline);

        // Tolerance: 5 MB.  The ring is pre-allocated to capacity at
        // construction; once warmed up, additional lines reuse slots.
        const TOLERANCE: u64 = 5 * 1024 * 1024;
        assert!(
            growth < TOLERANCE,
            "scrollback ring grew {} bytes ({}→{}); expected bounded",
            growth,
            baseline,
            after
        );

        // Sanity: scrollback is exactly capped at the configured capacity.
        assert_eq!(
            t.grid().scrollback_len(),
            t.grid().scrollback_capacity(),
            "scrollback should be at capacity after the soak"
        );
    }

    /// Sister of the in-RAM soak: build a Terminal with a disk-backed
    /// (anon-mmap) scrollback and feed millions of scrolled lines.
    /// Asserts:
    ///
    ///   1. RSS stays bounded — anon-mmap region is fixed-size, page
    ///      writes after warm-up reuse already-faulted pages, no leak
    ///      per scrolled line.
    ///   2. The ring's logical capacity stays at the configured cap
    ///      (RAM cap + disk cap) — wrap-around overwrites in place,
    ///      `len` never exceeds capacity.
    ///
    /// 10 M lines = ~7800 ring wraps at the small test-cap of ~1280
    /// total lines.  Catches drift from cumulative state that
    /// 1 M-line tests can hide (e.g. if a per-wrap operation
    /// allocates O(1) but with leaked drop, 10 M wraps shows it).
    ///
    /// Run via `bin/soak.sh`.
    #[test]
    #[ignore = "soak; run via bin/soak.sh"]
    fn soak_disk_scrollback_bounded_under_ten_million_lines() {
        use crate::scrollback::{Scrollback, LINES_PER_PAGE};

        let cols: u16 = 80;
        // Tight caps so we exercise the ring overwrite path heavily.
        let ram_cap = 256;
        let max_pages = 4; // 4 × 256 = 1024 lines on disk
        let scrollback = Scrollback::disk(ram_cap, max_pages, cols as usize)
            .expect("disk scrollback");

        let grid = Grid::with_scrollback_kind(cols, 24, scrollback);
        let mut t = Terminal {
            grid,
            saved_main: None,
            parser: Parser::new(),
            attrs: CellAttrs::default(),
            saved_cursor: None,
            scroll_top: 0,
            scroll_bot: 23, // grid is 24 rows here
            pending_response: Vec::new(),
            response_window: VecDeque::new(),
            response_burst_last_warn: None,
            cursor_key_application_mode: false,
            bracketed_paste_mode: false,
            cursor_visible: true,
            pending_wrap: false,
            predictions: VecDeque::new(),
            cluster_buf: String::new(),
            grapheme_cursor: crate::grapheme::GraphemeCursor::new(),
            predictions_hit: 0,
            predictions_miss: 0,
            generation: 0,
        };

        // Warm up: prime the ring + write a couple of disk pages.
        let warmup_lines = ram_cap + 2 * LINES_PER_PAGE;
        let park = b"\x1B[24;80H";
        let line = b"this is a fairly typical 50-character log line!\n";
        for _ in 0..warmup_lines {
            t.feed(line);
            t.feed(park);
        }
        let baseline_rss = current_rss_bytes();

        // Push 10 M more lines — ~7800 wraps of the test ring.
        for _ in 0..10_000_000 {
            t.feed(line);
            t.feed(park);
        }

        let after_rss = current_rss_bytes();
        let rss_growth = after_rss.saturating_sub(baseline_rss);

        // RSS tolerance: 5 MB.  Anon-mmap ring is fixed-size so
        // RSS shouldn't grow with line count past warm-up — under
        // memory pressure the kernel pages dirty regions out to swap
        // rather than to a named file, but the test machine has
        // plenty of RAM so no eviction is expected here either way.
        const RSS_TOL: u64 = 5 * 1024 * 1024;
        assert!(
            rss_growth < RSS_TOL,
            "RSS grew {rss_growth} bytes after 10M lines (baseline {baseline_rss}, after {after_rss})"
        );

        // Capacity-cap sanity: total stored == RAM cap + disk cap.
        let expected_cap = ram_cap + max_pages * LINES_PER_PAGE;
        assert_eq!(
            t.grid().scrollback_len(),
            expected_cap,
            "scrollback should be capped at RAM + disk cap"
        );
        assert_eq!(t.grid().scrollback_capacity(), expected_cap);
    }

    // ----- alt screen (?1049) ---------------------------------------------

    fn first_row_text(t: &Terminal) -> String {
        let g = t.grid();
        (0..g.cols()).map(|c| g.cell(c, 0).ch).collect()
    }

    #[test]
    fn dec_1049_h_enters_alt_blank_grid() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"hello\r\n");
        assert!(first_row_text(&t).starts_with("hello"));

        t.feed(b"\x1b[?1049h"); // enter alt
        // Alt grid is fresh: row 0 should be all spaces.
        assert_eq!(first_row_text(&t).trim_end(), "");
    }

    #[test]
    fn dec_1049_l_restores_main_contents_and_cursor() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"main_a\r\nmain_b");
        let cursor_before = t.grid().cursor();

        // Enter alt, write something only in alt.
        t.feed(b"\x1b[?1049h");
        t.feed(b"alt_only");
        assert_eq!(first_row_text(&t).trim_end(), "alt_only");

        // Exit — main grid + cursor restored.
        t.feed(b"\x1b[?1049l");
        assert!(first_row_text(&t).starts_with("main_a"));
        assert_eq!(t.grid().cursor(), cursor_before);
    }

    #[test]
    fn dec_25_l_hides_cursor_h_shows() {
        let mut t = Terminal::new(10, 5);
        assert!(t.cursor_visible());
        t.feed(b"\x1b[?25l");
        assert!(!t.cursor_visible());
        t.feed(b"\x1b[?25h");
        assert!(t.cursor_visible());
    }

    #[test]
    fn alt_screen_ignored_if_already_in_alt() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"main\r\n");
        t.feed(b"\x1b[?1049h");
        t.feed(b"first_alt");
        // Re-entering must NOT clobber the saved main grid.
        t.feed(b"\x1b[?1049h");
        t.feed(b"\x1b[?1049l");
        // Main is still there.
        assert!(first_row_text(&t).starts_with("main"));
    }

    #[test]
    fn alt_grid_resize_tracks_main() {
        let mut t = Terminal::new(20, 5);
        t.feed(b"\x1b[?1049h"); // alt mode
        t.resize(30, 8);
        // Cursor must be in-bounds in the active (alt) grid.
        let (c, r) = t.grid().cursor();
        assert!(c < 30 && r < 8);
        // Saved main grid resized too — exit and verify it's the new size.
        t.feed(b"\x1b[?1049l");
        assert_eq!(t.grid().cols(), 30);
        assert_eq!(t.grid().rows(), 8);
    }

    // -------- local-echo prediction tests --------------------------------

    #[test]
    fn predict_paints_cell_and_advances_cursor() {
        let mut t = Terminal::new(20, 3);
        assert!(t.predict_byte(b'a'));
        assert_eq!(t.grid().cell(0, 0).ch, 'a');
        assert_eq!(t.grid().cursor(), (1, 0));
        // Hit/miss counters: prediction is queued, neither yet.
        assert_eq!(t.predictions_hit, 0);
        assert_eq!(t.predictions_miss, 0);
    }

    #[test]
    fn predict_then_matching_echo_is_silently_consumed() {
        let mut t = Terminal::new(20, 3);
        t.predict_byte(b'a');
        t.predict_byte(b'b');
        // PTY echoes both verbatim — grid stays the same, predictions
        // confirmed.
        t.feed(b"ab");
        assert_eq!(t.grid().cell(0, 0).ch, 'a');
        assert_eq!(t.grid().cell(1, 0).ch, 'b');
        assert_eq!(t.grid().cursor(), (2, 0));
        assert_eq!(t.predictions_hit, 2);
        assert_eq!(t.predictions_miss, 0);
    }

    #[test]
    fn mismatch_rolls_back_all_predictions_then_feeds_byte() {
        let mut t = Terminal::new(20, 3);
        // Pre-state: cell(0,0) blank, cursor at (0,0).
        t.predict_byte(b'a');
        t.predict_byte(b'b');
        // PTY sends 'X' instead of expected 'a' — rollback both
        // predictions, then write 'X' at the original cursor.
        t.feed(b"X");
        assert_eq!(t.grid().cell(0, 0).ch, 'X');
        // The 'b' prediction's cell (col 1) is back to blank.
        assert_eq!(t.grid().cell(1, 0).ch, ' ');
        assert_eq!(t.grid().cursor(), (1, 0));
        assert_eq!(t.predictions_hit, 0);
        assert_eq!(t.predictions_miss, 2);
    }

    #[test]
    fn predict_refused_in_alt_screen() {
        let mut t = Terminal::new(20, 3);
        t.feed(b"\x1b[?1049h"); // enter alt screen
        assert!(!t.can_predict());
        assert!(!t.predict_byte(b'a'));
        // No grid mutation, no queue growth.
        assert_eq!(t.grid().cell(0, 0).ch, ' ');
        assert!(t.predictions.is_empty());
    }

    #[test]
    fn predict_refused_for_control_chars() {
        let mut t = Terminal::new(20, 3);
        // Tab, CR, LF, BS, ESC all unsafe — shell-side meaning varies.
        for b in [b'\t', b'\r', b'\n', 0x08u8, 0x1Bu8, 0x03u8] {
            assert!(!t.predict_byte(b), "byte {b:#04x} should not predict");
        }
        assert!(t.predictions.is_empty());
    }

    #[test]
    fn alt_screen_mid_feed_drops_pending_predictions() {
        let mut t = Terminal::new(20, 3);
        t.predict_byte(b'a');
        // PTY response opens alt screen — predictions are anchored to
        // the now-discarded main grid, so they get tossed.
        t.feed(b"\x1b[?1049h");
        assert!(t.predictions.is_empty());
        assert_eq!(t.predictions_miss, 1);
    }

    // ── Resize reflow gate ─────────────────────────────────────────
    //
    // Regression wall for the "resize 截断" class of bug: shrinking
    // the window used to truncate every row at the narrow width and
    // wipe scrollback, so a shrink → grow round trip (window drag,
    // shell self-update reopening at the default rect, attach-then-
    // resize bootstrap) permanently mangled content.  These tests pin
    // the reflow contract: ANY resize sequence is lossless for
    // logical content.

    /// One visible row as a trimmed string ('\0' wide-trail cells
    /// skipped, same as the clipboard serialiser).
    fn row_text(t: &Terminal, r: u16) -> String {
        let g = t.grid();
        let mut s: String = (0..g.cols())
            .map(|c| g.cell(c, r).ch)
            .filter(|&ch| ch != '\0')
            .collect();
        while s.ends_with(' ') {
            s.pop();
        }
        s
    }

    /// All logical content — scrollback then live rows, glued by the
    /// continuation flags, trailing blank lines dropped.  This is the
    /// width-independent invariant: it must survive any resize chain.
    fn logical_text(t: &Terminal) -> String {
        let g = t.grid();
        let mut lines: Vec<String> = Vec::new();
        let absorb = |row: String, wrapped: bool, lines: &mut Vec<String>| {
            let row = row.trim_end().to_string();
            if wrapped && !lines.is_empty() {
                lines.last_mut().unwrap().push_str(&row);
            } else {
                lines.push(row);
            }
        };
        for i in 0..g.scrollback_len() {
            let row: String = g
                .scrollback_line(i)
                .unwrap()
                .iter()
                .map(|c| c.ch)
                .filter(|&ch| ch != '\0')
                .collect();
            absorb(row, g.scrollback_wrapped(i), &mut lines);
        }
        for r in 0..g.rows() {
            let row: String = (0..g.cols())
                .map(|c| g.cell(c, r).ch)
                .filter(|&ch| ch != '\0')
                .collect();
            absorb(row, g.row_wrapped(r), &mut lines);
        }
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    #[test]
    fn autowrap_sets_continuation_flag() {
        let t = term_with(10, 5, b"0123456789ABCDE");
        assert!(!t.grid().row_wrapped(0));
        assert!(t.grid().row_wrapped(1), "autowrapped row must be flagged");
        assert_eq!(row_text(&t, 0), "0123456789");
        assert_eq!(row_text(&t, 1), "ABCDE");
        // Explicit newline does NOT flag.
        let t2 = term_with(10, 5, b"abc\r\ndef");
        assert!(!t2.grid().row_wrapped(1));
    }

    #[test]
    fn reflow_shrink_rewraps_instead_of_truncating() {
        let mut t = term_with(10, 5, b"0123456789ABCDE");
        t.resize(5, 5);
        assert_eq!(row_text(&t, 0), "01234");
        assert_eq!(row_text(&t, 1), "56789");
        assert_eq!(row_text(&t, 2), "ABCDE");
        assert!(t.grid().row_wrapped(1) && t.grid().row_wrapped(2));
        assert_eq!(logical_text(&t), "0123456789ABCDE");
    }

    #[test]
    fn reflow_grow_unwraps_back_to_one_row() {
        let mut t = term_with(10, 5, b"0123456789ABCDE");
        t.resize(5, 5);
        t.resize(20, 5);
        assert_eq!(row_text(&t, 0), "0123456789ABCDE");
        assert_eq!(row_text(&t, 1), "");
        assert!(!t.grid().row_wrapped(1));
        assert_eq!(logical_text(&t), "0123456789ABCDE");
    }

    #[test]
    fn reflow_roundtrip_with_scrollback_is_lossless() {
        // 40 numbered lines through a 24-row grid → 16+ lines live in
        // scrollback.  Mix of short lines and >80-char lines so both
        // the wrap and no-wrap paths are exercised.
        let mut t = Terminal::new(80, 24);
        for i in 0..40 {
            let line = if i % 7 == 0 {
                format!("line{:02}-{}\r\n", i, "x".repeat(100))
            } else {
                format!("line{:02}\r\n", i)
            };
            t.feed(line.as_bytes());
        }
        let before = logical_text(&t);
        assert!(t.grid().scrollback_len() > 0, "test needs scrollback");
        t.resize(37, 24);
        assert_eq!(logical_text(&t), before, "shrink must be lossless");
        t.resize(80, 24);
        assert_eq!(logical_text(&t), before, "grow back must be lossless");
        t.resize(13, 24);
        t.resize(200, 50);
        t.resize(80, 24);
        assert_eq!(logical_text(&t), before, "wild resize chain must be lossless");
    }

    #[test]
    fn rows_only_resize_preserves_scrollback_and_content() {
        let mut t = Terminal::new(20, 5);
        for i in 0..12 {
            t.feed(format!("l{:02}\r\n", i).as_bytes());
        }
        let before = logical_text(&t);
        t.resize(20, 3);
        assert_eq!(logical_text(&t), before, "rows shrink must be lossless");
        t.resize(20, 10);
        assert_eq!(logical_text(&t), before, "rows grow must be lossless");
    }

    #[test]
    fn reflow_never_splits_wide_pairs() {
        // 2 ASCII + 5 CJK (10 cols) + 2 ASCII = 14 columns at width 20.
        let mut t = term_with(20, 4, "AA你好世界啊BB".as_bytes());
        for w in [5u16, 7, 4, 20] {
            t.resize(w, 4);
            let g = t.grid();
            for r in 0..g.rows() {
                assert_ne!(
                    g.cell(0, r).ch,
                    '\0',
                    "row {r} at width {w} starts with a wide-trail cell"
                );
            }
            for i in 0..g.scrollback_len() {
                assert_ne!(g.scrollback_line(i).unwrap()[0].ch, '\0');
            }
            assert_eq!(logical_text(&t), "AA你好世界啊BB");
        }
    }

    #[test]
    fn reflow_keeps_cursor_on_its_logical_position() {
        // Prompt-like: short line, cursor right after it.
        let mut t = term_with(40, 10, b"$ echo hello");
        assert_eq!(t.grid().cursor(), (12, 0));
        t.resize(8, 10);
        // "$ echo hello" (12 chars) at width 8 → "$ echo h" / "ello";
        // cursor lands at the end of the continuation row.
        let (col, row) = t.grid().cursor();
        assert_eq!(row, 1);
        assert_eq!(col, 4);
        t.resize(40, 10);
        assert_eq!(t.grid().cursor(), (12, 0));
    }

    #[test]
    fn ed2_clear_screen_resets_continuation_flags() {
        let mut t = term_with(10, 5, b"0123456789ABCDE");
        assert!(t.grid().row_wrapped(1));
        t.feed(b"\x1b[2J");
        for r in 0..5 {
            assert!(!t.grid().row_wrapped(r), "row {r} flag survived ED2");
        }
    }

    #[test]
    fn reflow_preserves_blank_lines_between_content() {
        let mut t = term_with(20, 8, b"top\r\n\r\n\r\nbottom");
        let before = logical_text(&t);
        assert_eq!(before, "top\n\n\nbottom");
        t.resize(9, 8);
        t.resize(20, 8);
        assert_eq!(logical_text(&t), before);
    }
}
