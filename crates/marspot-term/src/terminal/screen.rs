//! Screen-level state changes: the alt screen, origin mode and the
//! row limits it sets, cursor save and restore, DECALN, and the soft
//! and hard resets.

use super::erase::{blank_with, fill_range};
use super::{Handler, MouseTrackingMode, SavedCursor, SavedMain, ScreenModes};
use crate::grid::{Cell, CellAttrs, DEFAULT_SCROLLBACK_LINES, Grid};

impl<'a> Handler<'a> {
    /// `?1049h` — switch to a fresh alternate screen, save the main
    /// grid + cursor.  No-op if already in alt mode.
    pub(super) fn enter_alt_screen(&mut self) {
        if self.saved_main.is_some() {
            return;
        }
        let cols = self.grid.cols();
        let rows = self.grid.rows();
        let cursor = self.grid.cursor();
        // VT spec says alt buffer has no scrollback,but user-perception
        // wise iTerm2 / Kitty / Alacritty all let scroll wheel browse
        // alt-screen history while inside a TUI(claudecode 长对话刷出
        // 屏顶 ≠ 没法再看).Use the same `DEFAULT_SCROLLBACK_LINES`
        // ring 主屏用 — claudecode 等 TUI 输出量大,小 ring 装不下
        // 一次会话.Drop wholesale on `?1049l`(exit_alt_screen 把整
        // 个 grid 替换回 saved_main).
        let mut alt = Grid::with_scrollback(cols, rows, DEFAULT_SCROLLBACK_LINES);
        // The switch does not move the cursor.  `?1049l` restores it
        // because the program moved it, not because the switch did.
        // A recording of `ESC 7` taken immediately after `?1049h`
        // settles it: the reference terminal saves the row the shell
        // was on, so the `ESC 8` at the end of that stream lands nine
        // rows down, where this used to put it at the top.
        alt.set_cursor(cursor.0, cursor.1);
        let main = std::mem::replace(self.grid, alt);
        std::mem::swap(self.saved_cursor, self.parked_saved_cursor);
        // The alt screen starts with the protocol off, and the main
        // screen's stack is put away with its grid.  A TUI that raises
        // the protocol and then dies cannot leave the shell it drops
        // back to encoding keys the shell does not read.
        let kitty_keyboard = std::mem::take(self.kitty_keyboard);
        *self.saved_main = Some(SavedMain {
            grid: main,
            cursor,
            kitty_keyboard,
        });
        self.relink();
    }

    /// `?1049l` — restore the saved main grid + cursor.  No-op if not
    /// in alt mode.
    /// DECSTR — soft reset (`CSI ! p`).
    ///
    /// Puts the terminal's modes back where a fresh program expects
    /// them and leaves the screen alone.  That division is the point
    /// of the sequence: a program recovering from a confused
    /// predecessor wants its modes back without losing what is on
    /// screen.
    ///
    /// Tab stops are deliberately not touched — DEC STD 070 leaves
    /// those to RIS, and a program that soft-resets in the middle of a
    /// table would otherwise lose its columns.
    /// Where row 1 is, and how far down rows are allowed to go.
    ///
    /// With DECOM off these are the screen's own extremes.  With it on
    /// they are the scroll region's, and a program addressing "row 1"
    /// means the first row of its region -- which is the whole point of
    /// the mode for anything drawing inside a window it set up.
    fn row_origin(&self) -> (u16, u16) {
        if *self.origin_mode {
            (*self.scroll_top, *self.scroll_bot)
        } else {
            (0, self.grid.rows().saturating_sub(1))
        }
    }

    /// Turn a row a program asked for into a row on the screen.
    pub(super) fn absolute_row(&self, requested: u16) -> u16 {
        let (top, bot) = self.row_origin();
        top.saturating_add(requested).min(bot)
    }

    /// The 1-based row to report back, in the origin the program is
    /// addressing in.
    pub(super) fn reported_row(&self, row: u16) -> u16 {
        let (top, _) = self.row_origin();
        row.saturating_sub(top) + 1
    }

    /// Push `[col..cols)` of a row right by `n`, dropping what falls
    /// off the end.  The vacated cells are left for the caller to
    /// write over.
    pub(super) fn shift_line_right(&mut self, col: u16, row: u16, n: u16) {
        let cols = self.grid.cols();
        if n == 0 || col >= cols {
            return;
        }
        for c in (col + n..cols).rev() {
            let src = self.grid.cell(c - n, row);
            self.grid.set_cell(c, row, src);
        }
        let blank = blank_with(*self.attrs);
        for c in col..(col + n).min(cols) {
            self.grid.set_cell(c, row, blank);
        }
    }

    /// How far up a relative move may go: the top margin when the
    /// cursor is inside the region, the top of the screen when it is
    /// already above it.
    pub(super) fn up_limit(&self, row: u16) -> u16 {
        if row >= *self.scroll_top { *self.scroll_top } else { 0 }
    }

    /// The mirror of `up_limit`.
    pub(super) fn down_limit(&self, row: u16) -> u16 {
        if row <= *self.scroll_bot {
            *self.scroll_bot
        } else {
            self.grid.rows().saturating_sub(1)
        }
    }

    pub(super) fn cursor_to_origin(&mut self) {
        let (top, _) = self.row_origin();
        self.grid.set_cursor(0, top);
        *self.pending_wrap = false;
    }

    /// DECSC, and the save half of `?1048` / `?1049`.
    ///
    /// One slot, shared: xterm defines `?1049` as "save cursor as in
    /// DECSC" and `?1048` as DECSC outright, so a program that saves
    /// with `ESC 7` and then enters the alternate screen finds its
    /// own save replaced.  Keeping a second slot would make `ESC 8`
    /// after an alt-screen round trip restore something no other
    /// terminal restores.
    pub(super) fn save_cursor(&mut self) {
        let (col, row) = self.grid.cursor();
        *self.saved_cursor = Some(SavedCursor {
            col,
            row,
            attrs: *self.attrs,
            pending_wrap: *self.pending_wrap,
            g0_graphics: *self.g0_graphics,
            origin_mode: *self.origin_mode,
        });
    }

    /// DECRC, and the restore half of `?1048` / `?1049`.  xterm-style
    /// no-op when nothing was ever saved.
    pub(super) fn restore_cursor(&mut self) {
        if let Some(s) = *self.saved_cursor {
            self.grid.set_cursor(s.col, s.row);
            *self.attrs = s.attrs;
            *self.pending_wrap = s.pending_wrap;
            *self.g0_graphics = s.g0_graphics;
            *self.origin_mode = s.origin_mode;
        }
    }

    /// DECALN — fill the screen with `E` and go home.
    pub(super) fn screen_alignment_pattern(&mut self) {
        let (cols, rows) = (self.grid.cols(), self.grid.rows());
        let attrs = CellAttrs::default();
        fill_range(self.grid, 0, cols as u32 * rows as u32, attrs);
        for row in 0..rows {
            for col in 0..cols {
                self.grid.set_cell(col, row, Cell { ch: 'E', attrs });
            }
        }
        self.grid.clear_all_wrapped();
        *self.scroll_top = 0;
        *self.scroll_bot = rows.saturating_sub(1);
        self.grid.set_cursor(0, 0);
        *self.pending_wrap = false;
    }

    /// DECCOLM — the part of it that is ours to do.
    ///
    /// The mode asks for 80 or 132 columns.  How wide a pane is is not
    /// this terminal's to decide: the window owns it, and a program
    /// that could reshape the window by printing four bytes would be a
    /// worse terminal, not a more compatible one.  What the mode also
    /// does is specified and is ours: the screen is cleared, the scroll
    /// region goes back to the full height, and the cursor goes home.
    /// Programs use DECCOLM for exactly that clearing side effect, and
    /// ignoring it leaves the old screen underneath whatever they draw
    /// next.
    pub(super) fn column_mode_reset(&mut self) {
        let (cols, rows) = (self.grid.cols(), self.grid.rows());
        fill_range(self.grid, 0, cols as u32 * rows as u32, CellAttrs::default());
        self.grid.clear_all_wrapped();
        *self.scroll_top = 0;
        *self.scroll_bot = rows.saturating_sub(1);
        self.grid.set_cursor(0, 0);
        *self.pending_wrap = false;
    }

    pub(super) fn soft_reset(&mut self) {
        *self.attrs = CellAttrs::default();
        *self.saved_cursor = None;
        *self.parked_saved_cursor = None;
        *self.cursor_visible = true;
        *self.cursor_shape = 0;
        *self.cursor_key_app_mode = false;
        *self.bracketed_paste = false;
        *self.sync_output = false;
        *self.alt_scroll = false;
        *self.focus_reporting = false;
        *self.focus_reported = None;
        *self.mouse_tracking_mode = MouseTrackingMode::Off;
        *self.mouse_sgr_encoding = false;
        *self.pending_wrap = false;
        *self.scroll_top = 0;
        *self.scroll_bot = self.grid.rows().saturating_sub(1);
        *self.origin_mode = false;
        *self.insert_mode = false;
        self.modes.autowrap = true;
        self.modes.keypad_app = false;
    }

    /// RIS — reset to initial state (`ESC c`).  What `reset` sends.
    ///
    /// Everything the soft reset does, and then the screen: out of the
    /// alternate buffer, scrollback gone, every cell blank, cursor
    /// home, tab stops back to every eighth column, title forgotten.
    ///
    /// The two marspot-specific switches — `u_tags` and
    /// `render_markup` — are not reset here, because this dispatcher
    /// does not hold them.  They are ours, no program sends RIS
    /// expecting them to move, and claiming to reset them from here
    /// would be a comment rather than code.
    pub(super) fn hard_reset(&mut self) {
        if self.saved_main.is_some() {
            self.exit_alt_screen();
        }
        self.soft_reset();
        let (cols, rows) = (self.grid.cols(), self.grid.rows());
        fill_range(self.grid, 0, cols as u32 * rows as u32, CellAttrs::default());
        self.grid.clear_all_wrapped();
        self.grid.clear_scrollback();
        self.grid.set_cursor(0, 0);
        *self.tabs = crate::tabs::TabStops::new(cols);
        *self.g0_graphics = false;
        // the palette keeps its generation moving, so a frame writer
        // never takes the reset table for one it already published
        let palette = std::mem::take(&mut self.modes.palette);
        *self.modes = ScreenModes { palette, ..ScreenModes::default() };
        self.modes.palette.reset_all();
        self.osc_title.clear();
        self.close_link();
    }

    pub(super) fn exit_alt_screen(&mut self) {
        if let Some(saved) = self.saved_main.take() {
            std::mem::swap(self.saved_cursor, self.parked_saved_cursor);
            *self.grid = saved.grid;
            self.grid.share_if_few();
            self.grid.set_cursor(saved.cursor.0, saved.cursor.1);
            *self.kitty_keyboard = saved.kitty_keyboard;
            self.relink();
        }
    }
}
