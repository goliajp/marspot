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

use crate::grid::{char_width, Cell, CellAttrs, Color, Grid, DEFAULT_SCROLLBACK_LINES};
use crate::parser::{Parser, ParserCallbacks};
use crate::scrollback::Scrollback;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Resolve the scrollback scratch directory once, the first time
/// any Terminal is constructed.  `None` ⇒ stay on RAM-only
/// scrollback for every session in this process; `Some(dir)` ⇒
/// every session opens a disk-backed scrollback in that directory.
///
/// Source order (first hit wins):
///   1. `MARS_DISK_SCROLLBACK` env var = explicit path → use it
///   2. `MARS_DISK_SCROLLBACK` env var = `1` → use the default
///      `~/Library/Caches/mars/scrollback`
///   3. unset → no disk scrollback
fn disk_scrollback_dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let v = std::env::var("MARS_DISK_SCROLLBACK").ok()?;
        let path = if v == "1" {
            let home = std::env::var("HOME").ok()?;
            PathBuf::from(home).join("Library/Caches/mars/scrollback")
        } else {
            PathBuf::from(v)
        };
        eprintln!("[mars] MARS_DISK_SCROLLBACK → {}", path.display());
        // Best-effort sweep of stale files left by previous mars
        // processes that crashed / SIGKILL'd / SIGTERM'd before
        // their Drop could fire (Rust on macOS doesn't run Drop on
        // signal-induced exit).  Each file is named
        // `mars-sb-<pid>-<nanos>.log`; if the pid isn't alive,
        // unlink it.
        if let Ok(entries) = std::fs::read_dir(&path) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with("mars-sb-") || !name.ends_with(".log") {
                    continue;
                }
                let stem = &name["mars-sb-".len()..name.len() - ".log".len()];
                let pid: Option<u32> = stem.split('-').next().and_then(|s| s.parse().ok());
                if let Some(pid) = pid {
                    // libc::kill(pid, 0) returns 0 if the pid exists.
                    let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
                    if !alive {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
        Some(path)
    })
    .as_ref()
}

/// In-RAM ring size when disk scrollback is active.  The renderer
/// hits this for every `cell_at_view` past the live grid, so
/// keeping the recent screenful in RAM avoids ever paging on
/// typical scrollback (page-up / mouse-wheel by a few rows).
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
    /// DEC mode 25 (DECTCEM) — when false, the renderer hides the cursor.
    cursor_visible: bool,
    /// Local-echo predictions awaiting PTY confirmation.  Each matching
    /// byte from `feed()` pops the front; the first mismatching byte
    /// rolls back the whole queue (restores cells + cursor in reverse
    /// order) and falls through to the parser.
    predictions: VecDeque<Prediction>,
    /// Diagnostics — predictions confirmed by an echo byte.
    pub predictions_hit: u64,
    /// Diagnostics — predictions rolled back on mismatch (or alt-screen
    /// invalidation).
    pub predictions_miss: u64,
}

struct SavedMain {
    grid: Grid,
    cursor: (u16, u16),
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        // If MARS_DISK_SCROLLBACK is set, every session gets a
        // disk-backed scrollback in the configured directory.  Falls
        // back to the in-RAM ring on any error (no panic — the user
        // just gets the bounded-RAM history).
        let scrollback = match disk_scrollback_dir() {
            Some(dir) => Scrollback::disk(
                dir,
                DISK_SCROLLBACK_RAM_LINES,
                DISK_SCROLLBACK_PAGES,
                cols as usize,
            )
            .unwrap_or_else(|e| {
                eprintln!(
                    "[mars] disk scrollback init failed ({e}); falling back to RAM-only"
                );
                Scrollback::memory(DEFAULT_SCROLLBACK_LINES, cols as usize)
            }),
            None => Scrollback::memory(DEFAULT_SCROLLBACK_LINES, cols as usize),
        };
        Self {
            grid: Grid::with_scrollback_kind(cols, rows, scrollback),
            saved_main: None,
            parser: Parser::new(),
            attrs: CellAttrs::default(),
            cursor_visible: true,
            predictions: VecDeque::new(),
            predictions_hit: 0,
            predictions_miss: 0,
        }
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.grid.resize(cols, rows);
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
            let cursor_visible = &mut self.cursor_visible;
            let mut handler = Handler { grid, saved_main, attrs, cursor_visible };
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
    }
}

struct Handler<'a> {
    grid: &'a mut Grid,
    saved_main: &'a mut Option<SavedMain>,
    attrs: &'a mut CellAttrs,
    cursor_visible: &'a mut bool,
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
            _ => {} // unhandled DEC private mode — silently skip
        }
    }
}

impl<'a> ParserCallbacks for Handler<'a> {
    fn print(&mut self, ch: char) {
        let w = char_width(ch);
        if w == 0 {
            // Zero-width / control marker — terminal already executes
            // C0 controls separately.  Nothing to draw or advance.
            return;
        }
        let cols = self.grid.cols();
        let rows = self.grid.rows();
        let (mut col, mut row) = self.grid.cursor();

        // A wide char at the last column can't fit. Wrap first, then print
        // at the start of the new row.
        if w == 2 && col + 1 >= cols {
            if row + 1 < rows {
                self.grid.set_cursor(0, row + 1);
            } else {
                self.grid.scroll_up(1, blank_with(*self.attrs));
                self.grid.set_cursor(0, rows - 1);
            }
            let next = self.grid.cursor();
            col = next.0;
            row = next.1;
        }

        // Lead cell carries the printable char.  For wide chars, the trail
        // cell stores NUL with the same attrs — the renderer skips drawing
        // its glyph (NUL is treated as blank), and the lead glyph extends
        // visually across both cells via its natural advance width.
        self.grid.set_cell(col, row, Cell { ch, attrs: *self.attrs });
        if w == 2 {
            self.grid.set_cell(col + 1, row, Cell { ch: '\0', attrs: *self.attrs });
        }

        let next_col = col + w as u16;
        if next_col < cols {
            self.grid.set_cursor(next_col, row);
        } else if row + 1 < rows {
            self.grid.set_cursor(0, row + 1);
        } else {
            self.grid.scroll_up(1, blank_with(*self.attrs));
            self.grid.set_cursor(0, rows - 1);
        }
    }

    fn execute(&mut self, byte: u8) {
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
                // bottom.  Does not change column (LNM mode unset).
                let (col, row) = self.grid.cursor();
                let rows = self.grid.rows();
                if row + 1 < rows {
                    self.grid.set_cursor(col, row + 1);
                } else {
                    self.grid.scroll_up(1, blank_with(*self.attrs));
                    // Cursor stays at the now-blank last row.
                }
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

    fn esc_dispatch(&mut self, _intermediates: &[u8], _byte: u8) {
        // ESC dispatch handled in later phases (RIS, charset switching, etc.)
    }

    fn csi_dispatch(&mut self, params: &[u16], intermediates: &[u8], byte: u8) {
        if intermediates == b"?" {
            // DEC private mode set/reset.  Each param is a separate mode.
            match byte {
                b'h' => {
                    for &p in params {
                        self.dec_mode(p, true);
                    }
                }
                b'l' => {
                    for &p in params {
                        self.dec_mode(p, false);
                    }
                }
                _ => {}
            }
            return;
        }
        if !intermediates.is_empty() {
            // Other private-marker sequences not implemented yet.
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
                if mode == 3 {
                    self.grid.clear_scrollback();
                } else {
                    erase_in_display(self.grid, col, row, cols, rows, mode, *self.attrs);
                }
            }
            b'K' => {
                // EL: erase in line.  Cursor not moved.
                // 0 (default) — from cursor (inclusive) to end of line
                // 1           — from start of line to cursor (inclusive)
                // 2           — entire line
                let mode = param_raw(params, 0, 0);
                erase_in_line(self.grid, col, row, cols, mode, *self.attrs);
            }
            b'm' => {
                // SGR: set graphic rendition.  Mutates self.attrs in place.
                apply_sgr(self.attrs, params);
            }
            _ => {} // scrolling, mode-set, etc. arrive in later phases
        }
    }

    fn osc_dispatch(&mut self, _data: &[u8]) {
        // OSC handlers (window title, hyperlinks, palette) — later phase.
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
fn blank_with(attrs: CellAttrs) -> Cell {
    Cell { ch: ' ', attrs }
}

fn fill_range(grid: &mut Grid, start: u32, end_exclusive: u32, attrs: CellAttrs) {
    let cols = grid.cols() as u32;
    let blank = blank_with(attrs);
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
            3 => attrs.italic = true,
            4 => attrs.underline = true,
            7 => attrs.reverse = true,
            22 => attrs.bold = false,
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

    #[test]
    fn plain_ascii_writes_cells_and_advances_cursor() {
        let t = term_with(80, 24, b"hi");
        assert_eq!(t.grid().cell(0, 0).ch, 'h');
        assert_eq!(t.grid().cell(1, 0).ch, 'i');
        assert_eq!(t.grid().cursor(), (2, 0));
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
        t.feed(b"abc"); // row 0 full, cursor at (3 wraps to row 1)
        assert_eq!(t.grid().cursor(), (0, 1));
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
    fn soak_scrollback_bounded_under_million_lines() {
        let mut t = Terminal::new(80, 24);
        // Warm up: fill the scrollback ring once so its memory stabilizes.
        let warmup = b"\x1B[24;80H\n".repeat(15_000);
        t.feed(&warmup);

        let baseline = current_rss_bytes();

        // Now feed a million more newlines worth of scroll churn.
        let line = b"this is a fairly typical 50-character log line!\n";
        let park = b"\x1B[24;80H";
        for _ in 0..1_000_000 {
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
}
