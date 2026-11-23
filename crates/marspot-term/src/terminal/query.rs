//! What the host reads off a `Terminal` and the small state changes
//! it makes from outside the byte stream: modes, title, clipboard,
//! bell, responses, focus, resize.

use super::{MouseTrackingMode, Osc8, ScreenModes, Terminal};
use crate::grid::{CellAttrs, Grid};

impl Terminal {
    /// How far the main screen's cursor has gone up since the last ask.
    pub fn take_cursor_reach(&mut self) -> Option<crate::grid::Reach> {
        self.saved_main.as_mut().map_or(&mut self.grid, |s| &mut s.grid).take_reach()
    }

    /// The main screen's cursor, its row numbered as a [`Reach`] is:
    /// on the alternate screen, the one put back when it is left.
    ///
    /// [`Reach`]: crate::grid::Reach
    pub fn main_cursor(&self) -> (u64, u16) {
        let (grid, (col, row)) = match &self.saved_main {
            Some(s) => (&s.grid, s.cursor),
            None => (&self.grid, self.grid.cursor()),
        };
        (grid.scroll_push_count() + u64::from(row), col)
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    /// Current snapshot generation, bumped at the end of every
    /// non-empty `feed()`.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Is the terminal inside the alternate screen (`?1049h`)?
    ///
    /// `saved_main` holds the main grid exactly while alt mode is
    /// active, so its presence IS the state — there is no separate
    /// flag that could drift out of sync with it.
    pub fn in_alt_screen(&self) -> bool {
        self.saved_main.is_some()
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// See [`ScreenModes`].
    pub fn screen_modes(&self) -> &ScreenModes {
        &self.screen_modes
    }

    /// Kitty keyboard flags in force on the active screen — what the
    /// key encoder needs to know.  See [`KittyKeyboardStack`].
    pub fn kitty_keyboard_flags(&self) -> u8 {
        self.kitty_keyboard.current()
    }

    /// Everything the key encoder reads off the terminal, in one
    /// value.  Collected here so a new mode reaches the encoder by
    /// growing this struct rather than every call site's signature.
    pub fn input_modes(&self) -> crate::input_core::TermModes {
        crate::input_core::TermModes {
            cursor_key_app: self.cursor_key_application_mode,
            bracketed_paste: self.bracketed_paste_mode.on,
            kitty_keyboard: self.kitty_keyboard.current(),
        }
    }

    /// DEC mode 1000/1002/1003 — app-level mouse reporting state.
    /// When non-`Off`, marspot must encode mouse wheel / click events
    /// as escape sequences and forward to the PTY so the TUI(claudecode
    /// 等)handles them internally instead of marspot trying to scroll
    /// terminal scrollback(which a TUI 永远没填,因为它 redraws in-
    /// place).
    pub fn mouse_tracking_mode(&self) -> MouseTrackingMode {
        self.mouse_tracking_mode
    }

    /// DEC mode 1006 — when true, mouse events encode as SGR
    /// (`\x1B[<{btn};{x};{y}M/m`).False = legacy X11(`\x1B[M btn x y`
    /// 3-byte payload,只能表达 cell 1-223).
    pub fn mouse_sgr_encoding(&self) -> bool {
        self.mouse_sgr_encoding
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
        self.bracketed_paste_mode.on
    }

    /// How many times the program turned bracketed paste off, wrapping.
    /// zsh does it as it takes a line, so a move since a return was
    /// typed says the return was read.
    pub fn bracketed_paste_offs(&self) -> u32 {
        self.bracketed_paste_mode.offs()
    }

    /// Did the program ask for the wheel to arrive as arrow keys?
    ///
    /// See `Terminal::alt_scroll`.  Only meaningful together with the
    /// alternate screen: that is the shape the mode was defined for,
    /// and a program that leaves it set on the main screen would
    /// otherwise take the wheel away from the terminal's own
    /// scrollback, which the user can actually see.
    pub fn alt_scroll_mode(&self) -> bool {
        self.alt_scroll && self.in_alt_screen()
    }

    /// Put the pen back to plain.
    ///
    /// The style a program is drawing with is its own business, and a
    /// terminal has no business second-guessing it — except that a
    /// style can be left on by something that is not the program, and
    /// then there is nothing to turn it off.  This is `reset`, scoped
    /// to the one thing that gets stuck.
    pub fn reset_attrs(&mut self) {
        self.attrs = CellAttrs::default();
        self.osc8 = Osc8::default();
    }

    /// Tell this terminal whether its program prints unrendered markup.
    pub fn set_render_markup(&mut self, on: bool) {
        self.render_markup = on;
    }

    /// Has a plugin said so?  Read by the session to log the change
    /// rather than the heartbeat that carries it.
    pub fn render_markup(&self) -> bool {
        self.render_markup
    }

    /// Is a synchronized update open (`CSI ? 2026 h` with no `l` yet)?
    ///
    /// The presenter should hold the frame while this is true — under
    /// its own timeout, since the terminal cannot make a program close
    /// what it opened.
    /// Does the program want focus in/out events (DEC 1004)?
    pub fn focus_reporting(&self) -> bool {
        self.focus_reporting
    }

    /// Tell the program the pane gained or lost focus, if it asked.
    ///
    /// `CSI I` / `CSI O`, xterm's focus-in / focus-out.  Queued on the
    /// same path as a capability reply, so it reaches the PTY through
    /// the caller's normal response flush.  A repeat of the state
    /// already reported sends nothing: L2 recomputes focus on more
    /// occasions than it changes.
    pub fn report_focus(&mut self, focused: bool) {
        if !self.focus_reporting || self.focus_reported == Some(focused) {
            return;
        }
        self.focus_reported = Some(focused);
        // Not routed through `record_response`: that guards against a
        // capability-query echo loop, and this is not a reply to the
        // program — nothing it sends can make us send more of these.
        self.pending_response
            .extend_from_slice(if focused { b"\x1b[I" } else { b"\x1b[O" });
    }

    /// Has this program ever opened a synchronized update?  Sticky —
    /// see [`Self::uses_sync_output`].
    /// The cursor shape the program asked for — see `cursor_shape`.
    pub fn cursor_shape(&self) -> u8 {
        self.cursor_shape
    }

    /// What the program last called itself (OSC 0 / OSC 2), empty if
    /// it never said.
    pub fn osc_title(&self) -> &str {
        &self.osc_title
    }

    pub fn uses_sync_output(&self) -> bool {
        self.uses_sync_output
    }

    pub fn sync_output_active(&self) -> bool {
        self.sync_output
    }

    /// Forget the modes that belong to the process that was running,
    /// keeping everything that belongs to the *content*.
    ///
    /// A resurrected session restores its snapshot onto a **brand-new
    /// shell**: the scrollback and screen are the point of the
    /// restore, but mouse tracking, SGR mouse encoding, bracketed
    /// paste and application cursor keys were switched on by a program
    /// that no longer exists.  Carrying them over makes the terminal
    /// lie about the fresh shell — and the lie is user-visible: with
    /// mouse tracking "on", a scroll gets encoded as `CSI < 64;x;y M`
    /// and typed straight into a zsh prompt, which answers
    /// `command not found: 29M64`.
    ///
    /// NOT for the execv handoff, where the shell survives and every
    /// one of these modes is still genuinely set.
    pub fn reset_process_owned_modes(&mut self) {
        self.mouse_tracking_mode = MouseTrackingMode::Off;
        self.mouse_sgr_encoding = false;
        self.bracketed_paste_mode.set(false);
        // A dead program cannot close its update; leaving this set
        // would hold the next program's first frame hostage.
        self.sync_output = false;
        // Likewise: the wheel goes back to the terminal's scrollback
        // when whoever claimed it is gone.
        self.alt_scroll = false;
        self.cursor_key_application_mode = false;
        // A dead program is not waiting to hear about focus.
        self.focus_reporting = false;
        self.focus_reported = None;
        // A new shell starts with a visible cursor; a TUI that hid it
        // is gone.
        self.cursor_visible = true;
        // And with no styling.  A program that switched underline or a
        // colour on and died there would otherwise hand the fresh
        // shell its own look — and unlike a mode, nothing about a
        // prompt necessarily turns it off again: a full-screen program
        // can run for hours without ever emitting `CSI 0 m`, so the
        // style rides every new cell until something happens to reset
        // it (measured 2026-09-06 on a pane left underlined by a bug
        // of mine: 300 KB of output, not one reset in it).  A link left
        // open is the same thing.
        self.attrs = CellAttrs::default();
        self.osc8 = Osc8::default();
        // the keypad, wrapping and a reversed screen are the program's too
        let m = &mut self.screen_modes;
        (m.keypad_app, m.autowrap, m.reverse_screen) = (false, true, false);
    }

    /// Take whatever a program asked to put on the clipboard.  The
    /// terminal cannot reach a pasteboard, so it holds the text until
    /// the layer that can collects it, once.
    pub fn take_osc_clipboard(&mut self) -> Option<String> {
        self.osc_clipboard.take()
    }

    /// Whether a bare BEL arrived since the last call; any number of
    /// them is one ring.  A BEL that ends an OSC string is not one.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell)
    }

    /// Forget that anything asked for mouse reports.
    ///
    /// For when marspot itself takes the foreground program down.  A
    /// signal produces no bytes, so a `SIGTERM`ed TUI never sends the
    /// `CSI ? 1002 l` it would have sent on a clean exit, and this
    /// terminal goes on believing a program that no longer exists
    /// wants mouse reports.  What the pane actually holds by then is a
    /// shell prompt, and the next scroll gets encoded as
    /// `CSI < 64;x;y M` and typed into it.
    ///
    /// Narrower than [`reset_process_owned_modes`](Self::reset_process_owned_modes)
    /// on purpose.  That one is for a snapshot restored onto a
    /// brand-new shell, where *nothing* on screen set any of these.
    /// Here the shell is the same shell it always was: it owns its own
    /// bracketed paste and application cursor keys and re-asserts them
    /// per prompt, so clearing those would break a paste already in
    /// flight for no gain.  Mouse reporting is the one mode a shell
    /// never sets and a dead TUI never clears.
    pub fn reset_mouse_reporting(&mut self) {
        self.mouse_tracking_mode = MouseTrackingMode::Off;
        self.mouse_sgr_encoding = false;
    }

    /// Drain any bytes the terminal wants to send back to the PTY in
    /// response to capability / version queries (CSI c, CSI > 0 c,
    /// CSI > 0 q, …). Caller (Session::pump) writes them to the PTY
    /// after the feed cycle so the app's `read()` returns them.
    /// Whether this pane has ever been flagged for a sustained storm
    /// of capability responses.  See `response_window`: a program
    /// starting up bursts and stops, a loop does not, and only the
    /// second one sets this.
    pub fn response_burst_warned(&self) -> bool {
        self.response_burst_last_warn.is_some()
    }

    pub fn take_response(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_response)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.grid.resize(cols, rows);
        self.tabs.resize(cols);
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
}
