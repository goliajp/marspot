// CARVE-OUT: pre-existing god file: the VT event to grid mutation handler; to be split by concern; shrink on every touch
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

mod bulk;
mod erase;
mod glyph;
mod kitty;
pub use kitty::{KITTY_KEYBOARD_SUPPORTED, KittyKeyboardStack};
use erase::{blank_with, erase_in_display, erase_in_line};
#[cfg(test)]
mod diff_tests;
mod lane;
mod modes;
mod new;
pub use modes::{PaletteOverrides, ScreenModes};
mod osc;
mod predict;
mod print;
mod query;
pub(crate) use osc::Osc8;
mod reference;
mod screen;
mod sgr;
use sgr::{apply_sgr, dec_special_graphic, param, param_raw};
mod scrollback_page;
mod snapshot_apply;
mod snapshot_clusters;
mod snapshot_wire;
mod snapshot_write;
pub use snapshot_wire::{deserialize_attrs_pub, serialize_attrs_pub};
#[cfg(test)]
use snapshot_wire::{ATTRS_BYTES, CELL_BYTES, serialize_attrs};
mod trace;
mod wide_class;

use trace::trace_seq;
#[cfg(test)]
use lane::{boring_width, fast_pict_width, fast_width};
#[cfg(test)]
use crate::grid::Color;
use crate::grid::{Cell, CellAttrs, Grid};
use crate::parser::{Parser, ParserCallbacks};
use crate::{lx_debug, lx_debug_sampled, lx_info, lx_warn};
use std::collections::VecDeque;
#[cfg(test)]
use std::io;
use std::time::{Duration, Instant};

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
    /// When the guess was made.  A prediction nothing ever answers
    /// has to expire on its own, or it stays painted forever — the
    /// shape of a `sudo` password prompt, which echoes nothing at
    /// all until Enter.
    at: std::time::Instant,
}

/// DEC mode 1000/1002/1003 — app-level mouse reporting.  `Off` 时
/// terminal 自己消化 wheel / 鼠标点击;Some 时 marspot 把它们 encode
/// 成 escape sequence 写回 PTY 让 TUI(claudecode 等)处理.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseTrackingMode {
    Off,
    /// `?1000` — press + release only.
    X11,
    /// `?1002` — X11 + drag motion(button held)events.
    ButtonEvent,
    /// `?1003` — ButtonEvent + plain motion(no button).
    AnyEvent,
}

/// Sweep the cluster pool once it is holding more than this.
///
/// A screen cannot point at more clusters than it has cells, and the
/// pool only grows where a cluster is written, so a bound a few
/// screens wide means the sweep runs rarely and the pool never
/// wanders.  Not a timer: an idle grid does no work at all.
const CLUSTER_POOL_SWEEP_AT: usize = 4096;


/// What marspot answers DA1 (and DECID) with: a VT100 with advanced
/// video, the answer the borrowed `xterm-256color` entry declares as
/// `u8` to its `u9` query.  A program that gets no answer stalls on its
/// capability probe and then falls back to a degraded rendering path.
const DA1_REPLY: &[u8] = b"\x1b[?1;2c";

/// Responses inside the rolling 100 ms window that count as a storm.
const RESPONSE_BURST_AT: usize = 5;
/// How long the window has to stay over [`RESPONSE_BURST_AT`] before
/// it is a loop rather than a program starting up.
const RESPONSE_BURST_SUSTAINED: std::time::Duration = std::time::Duration::from_secs(1);

pub struct Terminal {
    grid: Grid,
    /// Saved main grid while the terminal is in alt-screen mode (`?1049h`).
    /// `Some` ⇒ alt mode active and `grid` is the alt buffer; `None` ⇒
    /// normal mode and `grid` is the only buffer.  When we exit alt mode
    /// the saved cursor goes with this Grid.
    saved_main: Option<SavedMain>,
    /// See [`KittyKeyboardStack`].  This is the ACTIVE screen's stack;
    /// the inactive one rides in `saved_main`.
    kitty_keyboard: KittyKeyboardStack,
    parser: Parser,
    /// Current SGR state — every printed glyph (and every BCE-erased cell)
    /// is stamped with this snapshot.  Persists across `feed` calls.
    attrs: CellAttrs,
    /// The OSC 8 link printed glyphs belong to.  Apart from `attrs`
    /// because it is not SGR: `CSI 0 m` does not end a link, and an
    /// erased cell is not part of one.
    osc8: Osc8,
    /// Cursor + SGR snapshot saved by ESC 7 (DECSC) or CSI s (SCO save);
    /// restored by ESC 8 (DECRC) or CSI u (SCO restore). Cleared by neither —
    /// TUI apps like claudecode lean on save/restore to redraw progress
    /// regions in place. `None` until first save; restore with no prior
    /// save is a no-op (matches xterm).
    saved_cursor: Option<SavedCursor>,
    /// The saved cursor belonging to the screen buffer that is *not*
    /// current.  There is one slot per buffer, as there is one grid per
    /// buffer: `ESC 7` inside a full-screen program saves where that
    /// program is, and it is still there when the program comes back,
    /// having gone to the main screen and returned.  The two swap on
    /// every switch, so `saved_cursor` is always the live buffer's.
    parked_saved_cursor: Option<SavedCursor>,
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
    /// driver.  The window is intentionally per-Terminal — nine panes
    /// loop in parallel show up as nine warnings, which is what we
    /// want.
    ///
    /// Crossing the threshold is not the signal; **staying** over it
    /// is.  A program starting up asks the same question several times
    /// in a row -- claudecode sends five `CSI ? u` inside 100 ms while
    /// it pushes keyboard flags -- and that was warning on every agent
    /// restart.  54 of them in one log, all of them a TUI booting, and
    /// among that a real loop would have gone unread.  A loop does not
    /// stop, so the warning waits for it not to.
    response_window: VecDeque<Instant>,
    /// When the window first went over the threshold and stayed there,
    /// or `None` while it is under.  See `response_window`.
    response_burst_since: Option<Instant>,
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
    /// Draw `<u>…</u>` as underline — `appearance.render_u_tags`.
    ///
    /// Cached per feed rather than read per character: the setting is
    /// a global behind a lock, and `print` is the hottest loop in the
    /// terminal.
    u_tags: bool,
    /// Did a plugin say this pane's program prints markup it does not
    /// render?  See `MsgType::PaneRenderMarkup`.
    render_markup: bool,
    /// Are we inside an unclosed `<u>`, collecting what it wraps?
    ///
    /// Only a MATCHED pair styles anything.  Turning underline on at
    /// the opening tag underlines everything after an unmatched one —
    /// which is any text that merely mentions the tag, and that is
    /// most of a conversation about this feature; reported against my
    /// own output within minutes of shipping it (2026-09-06).
    ///
    /// So the span is withheld until `</u>` arrives.  If it does not —
    /// a line feed, an escape sequence, or more text than
    /// `U_SPAN_CAP` — the opening tag and everything after it are
    /// printed exactly as they came.  The failure is then the
    /// behaviour from before the feature existed, which is the only
    /// safe thing for it to fall back to.
    u_open: bool,
    /// Characters held back while `<`… might turn out to be a tag.
    ///
    /// At most four (`</u>`).  Anything that stops being a prefix is
    /// printed, in order, exactly as it arrived.
    u_buf: String,
    /// DEC mode 1007 — alternate scroll mode.
    ///
    /// The program's own statement that, on this screen, the wheel
    /// means the arrow keys.  xterm made it up for exactly the case
    /// marspot was solving by hand: a full-screen view with history
    /// the terminal cannot move, so the wheel has to reach the program
    /// as keys instead.
    ///
    /// codex sets it the moment its transcript opens
    /// (`?1049h · ?1007h · …`) and clears it on the way out — so it
    /// answers "is the scroll view open" outright, where scanning the
    /// screen for a heading only guesses.  And it is not codex's
    /// alone: `less`, `man` and any pager that asks gets the same
    /// wheel for free, with no plugin that has to recognise them.
    alt_scroll: bool,
    /// DEC mode 2026 — synchronized output.
    ///
    /// A program brackets a whole repaint with `CSI ? 2026 h` … `l` to
    /// say "do not show anyone a half-drawn screen".  codex uses it for
    /// every frame (8,493 pairs in one session's byte log); without it
    /// the intermediate states reach the display and opening its
    /// transcript flashes black (2026-09-06, against iTerm2 which
    /// honours the mode).
    ///
    /// This is only the state.  The clock lives with whoever presents:
    /// a program that never closes its update must not freeze the pane,
    /// so the publisher holds frames under a timeout.
    sync_output: bool,
    /// DEC 1004 — the program asked to be told when this pane gains or
    /// loses focus.  It is a request, and until now it was accepted
    /// and then never answered: codex turns it on and leaves it on
    /// (measured on a real session — 12 sets, 16 resets, ON at the end
    /// of the stream), so it has been waiting the whole time.
    focus_reporting: bool,
    /// Last focus state actually reported, so a repeated notification
    /// from L2 does not become a repeated escape into the program.
    focus_reported: Option<bool>,
    /// DECSCUSR shape the program last asked for: 0 = the terminal's
    /// own default, 1/2 block, 3/4 underline, 5/6 bar, even = blinking.
    /// Nothing draws it yet — see the DECSCUSR arm for why.
    cursor_shape: u8,
    /// The last title the program set via OSC 0 / OSC 2.  Stored so it
    /// is available to whatever decides to show it; nothing displays
    /// it today (see `osc.rs`).
    osc_title: String,
    /// Sticky: this program has used DEC 2026 at least once.  Callers
    /// that must cut a byte stream at a consistent screen ask this
    /// first, so a pane that never synchronises (a shell, vim) never
    /// pays for the scan.
    uses_sync_output: bool,
    /// DEC mode 25 (DECTCEM) — when false, the renderer hides the cursor.
    cursor_visible: bool,
    /// DEC mode 1000 (X11) / 1002 (button-event) / 1003 (any-event) —
    /// TUI 进入 alt-screen 后通常 set;app(claudecode 等)期望从 PTY
    /// 收到 mouse 事件 escape sequence,marspot 看到后必须 forward
    /// 而不是按 wheel→scrollback 处理.None = off,Some(mode) = which
    /// reporting mode.之前(176e4f4 extract 后)全部 stub 成 no-op,
    /// claudecode 内 wheel 滚不动就是这个原因.
    mouse_tracking_mode: MouseTrackingMode,
    /// DEC mode 1006 — SGR encoding(`\x1B[<{btn};{x};{y}M/m`);
    /// alternative encodings(1015 urxvt、1005 utf-8 deprecated)等
    /// 现在不支持,claudecode 等现代 TUI 用 1006.
    mouse_sgr_encoding: bool,
    /// DECAWM "deferred wrap" — set after printing a glyph in the last
    /// column. The cursor visually stays put; the next print first wraps
    /// to a new row, the next non-print op (CR/LF/cursor move) clears
    /// the flag. Without this, drawing a box's right border followed by
    /// `\r\n` advances TWO rows instead of one — visible as extra blank
    /// rows between every row of TUI content (claudecode welcome box).
    pending_wrap: bool,
    /// Where the tabs are.  A tab used to be a no-op here, so anything
    /// that laid its output out with them came out with its columns
    /// collapsed — and the kernel does not help: a pseudo-terminal
    /// leaves `OXTABS` off, so the tab arrives verbatim.
    tabs: crate::tabs::TabStops,
    /// Text a program asked to put on the clipboard with OSC 52,
    /// waiting for whoever owns the pasteboard to collect it.
    osc_clipboard: Option<String>,
    /// A bare BEL arrived since the last `take_bell`.
    bell: bool,
    /// G0 is the DEC Special Graphics set, selected by `ESC ( 0`.
    g0_graphics: bool,
    /// DECOM (`?6`).  While set, row addressing counts from the top
    /// margin rather than the top of the screen, and the cursor cannot
    /// leave the scroll region.
    origin_mode: bool,
    /// IRM (`CSI 4 h`).  While set, a printed glyph pushes the rest of
    /// the line right instead of overwriting.
    insert_mode: bool,
    /// See [`ScreenModes`].
    screen_modes: ScreenModes,
    /// Rollbacks in a row.  A program that draws its own input
    /// somewhere else — codex, and any full-screen TUI that skips the
    /// alternate screen — never confirms a prediction, so the guess
    /// lands at the terminal cursor, moves it, and is wiped by the
    /// next repaint.  Measured 2026-09-07: at a zsh prompt 16
    /// keystrokes gave 15 hits and 1 miss; inside codex, 23 keystrokes
    /// gave 0 hits and 23 misses.  Not a judgement call — a tally.
    predict_misses: u32,
    /// Keystrokes declined since giving up, so one can be let through
    /// to find out whether the program changed underneath.
    predict_declined: u32,
    /// Local-echo predictions awaiting PTY confirmation.  Each matching
    /// byte from `feed()` pops the front; the first mismatching byte
    /// rolls back the whole queue (restores cells + cursor in reverse
    /// order) and falls through to the parser.
    predictions: VecDeque<Prediction>,
    /// Predictions that timed out, still owed an answer.  Their cells
    /// are already restored; the bytes stay here so that a late echo
    /// is recognised for what it is — evidence the program echoes
    /// after all, just slowly (ssh over a real link), rather than
    /// counted as a miss and used to switch prediction off on
    /// exactly the connection where it helps most.
    shadow: VecDeque<(u8, std::time::Instant)>,
    /// Smoothed round trip from keystroke to its echo.  Seeds the
    /// expiry deadline, so a slow link widens its own window instead
    /// of losing local echo entirely.
    echo_srtt: Duration,
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
    /// The buffered codepoint's fast-path width, when there is exactly
    /// one and it had one.  `fast_width` is a table lookup — the
    /// comment on `fast_pict_width` records it at 23 % of emoji parse
    /// time — and the old fast path paid it TWICE per glyph: once to
    /// classify the incoming char, and again next time round to
    /// re-classify the same char after decoding it back out of the
    /// String.  Carrying the answer forward costs a `u8`.
    ///
    /// Invariant: `Some((c, w))` iff `cluster_buf` holds exactly `c`
    /// and `fast_width(c) == Some(w)`.  Every site that touches
    /// `cluster_buf` maintains it; `debug_assert`s in the fast path
    /// hold it to that.
    cluster_fast: Option<(char, u8)>,
    /// Where the cluster now being built was written, and how wide it
    /// is.  A glyph is committed as soon as its width is known; a
    /// codepoint that arrives afterwards and belongs to the same
    /// cluster amends THIS cell instead of taking one of its own.
    ///
    /// The terminal used to hold a codepoint back for one round
    /// instead, so a variation selector or ZWJ arriving next could
    /// join before anything was drawn.  That cost 22.6 % of emoji
    /// parse (measured by ablation), and it did not even work across
    /// a feed boundary: the end-of-feed flush wrote the base and moved
    /// the cursor, so `a⚠️b` split after `⚠` put the VS16 in a cell of
    /// its own.  A pty ends its reads wherever the kernel had a break,
    /// so that was not exotic.
    ///
    /// `None` means there is no cluster to extend — the start of a
    /// feed, or after anything that moved the cursor.
    cluster_anchor: Option<(u16, u16, u8)>,
    grapheme_cursor: crate::grapheme::GraphemeCursor,
    /// Whether `grapheme_cursor`'s run state currently reflects
    /// `cluster_buf`.  The ASCII fast path in `Handler::print` skips
    /// the segmenter entirely (two printable-ASCII neighbours have an
    /// unconditional UAX #29 boundary — no GB rule joins Other+Other)
    /// and flips this false; the slow path re-seeds the cursor by
    /// replaying the (≤1 char) buffer before consulting it.  The
    /// fast path is what keeps a `cat` of plain text from paying the
    /// per-codepoint gbp/incb/pictographic table walks — measured
    /// ~84 % of parse time before it existed (2026-07-11 samply on
    /// cat-ascii).
    seg_synced: bool,
    /// Diagnostics — predictions confirmed by an echo byte.
    pub predictions_hit: u64,
    /// Diagnostics — predictions rolled back on mismatch (or alt-screen
    /// invalidation).
    pub predictions_miss: u64,
    /// Diagnostics — predictions rolled back because nothing answered
    /// them in time.  Distinct from a miss: the program may still be
    /// about to echo.
    pub predictions_expired: u64,
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
    /// The main screen's own keyboard stack.  See
    /// [`KittyKeyboardStack`] — the protocol gives each screen buffer
    /// its own, so a TUI that raises the protocol on entering the alt
    /// screen cannot leave it raised over the shell it returns to.
    kitty_keyboard: KittyKeyboardStack,
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
    /// The DECAWM deferred wrap in force when the cursor was saved.
    /// Restoring a cursor that sat in the last column has to restore
    /// the wrap it had armed, or the glyph after the restore lands on
    /// top of that column instead of on the next row.
    pending_wrap: bool,
    /// Whether G0 was the DEC special graphics set.  The saved cursor
    /// carries the character set, so a program that designates
    /// graphics, draws a box, and restores comes back to the set it
    /// had -- otherwise every letter it prints afterwards arrives as
    /// line-drawing.
    g0_graphics: bool,
    /// DECOM, which the saved cursor carries for the same reason: a
    /// restore that put the cursor back without its addressing mode
    /// would send the program's next move to a different row.
    origin_mode: bool,
}

impl Terminal {
    /// Feed bytes from the PTY through the parser, applying their effects
    /// to the grid.  Each byte is first checked against the front-of-queue
    /// prediction: a match silently consumes both (the prediction already
    /// painted the result), a mismatch rolls back the whole prediction
    /// queue and feeds the byte normally through the parser.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.feed_with(bytes, true);
    }

    /// Current snapshot generation.  Bumped at the end of every non-
    /// empty `feed()` call.  RFC-002 step 6 / step 8 compare it
    /// against a `last_pushed_generation` to decide whether to push.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

struct Handler<'a> {
    grid: &'a mut Grid,
    saved_main: &'a mut Option<SavedMain>,
    attrs: &'a mut CellAttrs,
    /// See `Terminal::osc8`.
    osc8: &'a mut Osc8,
    /// `osc8.id` by value, kept equal to it: tested once a run of text.
    link: u16,
    saved_cursor: &'a mut Option<SavedCursor>,
    /// See `Terminal::parked_saved_cursor`.
    parked_saved_cursor: &'a mut Option<SavedCursor>,
    scroll_top: &'a mut u16,
    scroll_bot: &'a mut u16,
    pending_response: &'a mut Vec<u8>,
    response_window: &'a mut VecDeque<Instant>,
    /// See `Terminal::response_burst_since`.
    response_burst_since: &'a mut Option<Instant>,
    response_burst_last_warn: &'a mut Option<Instant>,
    cursor_key_app_mode: &'a mut bool,
    bracketed_paste: &'a mut bool,
    /// DEC 2026 — see `Terminal::sync_output`.
    sync_output: &'a mut bool,
    /// Sticky companion to `sync_output` — see `Terminal::uses_sync_output`.
    uses_sync_output: &'a mut bool,
    /// See `Terminal::focus_reporting`.
    focus_reporting: &'a mut bool,
    focus_reported: &'a mut Option<bool>,
    /// See `Terminal::osc_title`.
    osc_title: &'a mut String,
    /// See `Terminal::cursor_shape`.
    cursor_shape: &'a mut u8,
    /// DEC 1007 — see `Terminal::alt_scroll`.
    alt_scroll: &'a mut bool,
    /// `appearance.render_u_tags` — see `Terminal::u_tags`.
    u_tags: bool,
    u_buf: &'a mut String,
    /// See `Terminal::u_open`.
    u_open: &'a mut bool,
    cursor_visible: &'a mut bool,
    /// See [`KittyKeyboardStack`].
    kitty_keyboard: &'a mut KittyKeyboardStack,
    mouse_tracking_mode: &'a mut MouseTrackingMode,
    mouse_sgr_encoding: &'a mut bool,
    pending_wrap: &'a mut bool,
    tabs: &'a mut crate::tabs::TabStops,
    osc_clipboard: &'a mut Option<String>,
    bell: &'a mut bool,
    g0_graphics: &'a mut bool,
    /// See `Terminal::origin_mode`.
    origin_mode: &'a mut bool,
    /// See `Terminal::insert_mode`.
    insert_mode: &'a mut bool,
    modes: &'a mut ScreenModes,
    cluster_buf: &'a mut String,
    /// See `Terminal::cluster_fast`.
    cluster_fast: &'a mut Option<(char, u8)>,
    /// See `Terminal::cluster_anchor`.
    cluster_anchor: &'a mut Option<(u16, u16, u8)>,
    grapheme_cursor: &'a mut crate::grapheme::GraphemeCursor,
    seg_synced: &'a mut bool,
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
        if self.response_window.len() < RESPONSE_BURST_AT {
            // Under the threshold: whatever it was, it stopped.
            *self.response_burst_since = None;
            return;
        }
        let since = *self.response_burst_since.get_or_insert(now);
        if now.duration_since(since) < RESPONSE_BURST_SUSTAINED {
            return;
        }
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
                sustained_ms = now.duration_since(since).as_millis() as u64,
                kind = kind
            );
            *self.response_burst_last_warn = Some(now);
        }
    }
}

impl<'a> ParserCallbacks for Handler<'a> {
    fn print(&mut self, ch: char) {
        // `<u>…</u>` becomes underline — but only as a MATCHED pair.
        //
        // Recognised HERE and not over the byte stream, because a byte
        // pass cannot tell text from the inside of an escape sequence:
        // `CSI < u`, the kitty-keyboard pop codex sends at startup,
        // carries the same characters.  By this point the parser has
        // already separated the two.
        //
        // One predictable branch on the hot path when the setting is
        // off, two when it is on; nothing is buffered until a literal
        // `<` actually arrives.
        if self.u_tags && (*self.u_open || ch == '<' || !self.u_buf.is_empty()) {
            self.u_step(ch);
            return;
        }
        let ch = if *self.g0_graphics { dec_special_graphic(ch) } else { ch };
        self.print_glyph(ch);
    }

    fn execute(&mut self, byte: u8) {
        // a control ends the text run: a `<` waiting to become a tag
        // never will, else a line ending in one swallows it
        self.flush_u_buf();
        self.flush_cluster_for_break();
        // row-advancers (LF/CR/BS/Tab): plain `\n` / `\r` show only here
        if matches!(byte, 0x08..=0x0D) {
            trace_seq("C0", &[], &[], byte);
        }
        // The deferred wrap is cancelled per control, not by all of
        // them (see `csi_dispatch`): ghostty clears it in
        // `carriageReturn`, `index` and `backspace` only; HT keeps it.
        match byte {
            0x08 => {
                *self.pending_wrap = false;
                // BS: cursor left one column, clamped at column 0.  Does
                // not erase the cell.
                let (col, row) = self.grid.cursor();
                if col > 0 {
                    self.grid.set_cursor(col - 1, row);
                }
            }
            0x0A..=0x0C => {
                *self.pending_wrap = false;
                // LF / VT / FF are IND with no carriage return.
                self.index();
            }
            0x0D => {
                *self.pending_wrap = false;
                // CR: cursor to column 0 of current row.
                let (_col, row) = self.grid.cursor();
                self.grid.set_cursor(0, row);
            }
            0x09 => {
                // HT.  The deferred wrap is deliberately NOT cleared:
                // a tab does not move to a new row, and ghostty keeps
                // it here too.
                let (col, row) = self.grid.cursor();
                let to = self.tabs.next(col, 1);
                self.grid.set_cursor(to, row);
            }
            0x07 => *self.bell = true,
            _ => {} // unknown C0 control: ignore for now
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], byte: u8) {
        self.flush_u_buf();
        self.flush_cluster_for_break();
        // No blanket clear of the deferred wrap here: an escape that
        // does not move the cursor must not drop it (see `csi_dispatch`).
        match byte {
            // HTS — set a tab stop at the cursor's column.
            b'H' if intermediates.is_empty() => {
                let (col, _) = self.grid.cursor();
                self.tabs.set(col);
            }
            // DECSC — save cursor (position + SGR attrs + deferred wrap).
            //
            // Guarded on having no intermediate: `ESC # 8` is DECALN,
            // not DECRC, and without the guard it was being restored
            // as a cursor.
            b'7' if intermediates.is_empty() => self.save_cursor(),
            // DECRC — restore cursor. xterm-style no-op when no save exists.
            b'8' if intermediates.is_empty() => self.restore_cursor(),
            // IND (ESC D) — index.  NEL (ESC E) — index with a
            // carriage return.  RI (ESC M) — reverse index.
            //
            // All three were falling through to the catch-all and being
            // discarded, which is invisible until a program uses one to
            // move its own screen.  codex does: it emits five `ESC M`
            // to open room at the top of its scroll region, then paints
            // everything below at absolute rows.  With the five ignored
            // its layout sat five rows lower in marspot than in the
            // program's own model, so its next partial repaint wrote
            // the status line over a row that still held `tab to queue
            // message` — the "`• Workingqu21e message`" report of
            // 2026-09-08.  Cross-checked against tmux on the identical
            // byte stream: same bytes, clean status row there.
            b'D' => {
                *self.pending_wrap = false;
                self.index();
            }
            b'E' => {
                *self.pending_wrap = false;
                let (_, row) = self.grid.cursor();
                self.grid.set_cursor(0, row);
                self.index();
            }
            b'M' => {
                *self.pending_wrap = false;
                self.reverse_index();
            }
            // DECID (ESC Z) — the pre-DA1 way of asking the same
            // question, still sent by older curses builds.  xterm
            // answers it with the DA1 string, so we do too.
            b'Z' => {
                self.pending_response.extend_from_slice(DA1_REPLY);
                self.record_response("DECID");
            }
            // DECKPAM (=) / DECKPNM (>) — application / normal keypad.
            b'=' | b'>' if intermediates.is_empty() => self.modes.keypad_app = byte == b'=',
            // HP memory lock / unlock, as xterm does them: the rows
            // above the cursor stop scrolling by becoming the part of
            // the screen above the top margin.
            b'l' if intermediates.is_empty() => {
                let (_, row) = self.grid.cursor();
                if row < *self.scroll_bot {
                    *self.scroll_top = row;
                }
            }
            b'm' if intermediates.is_empty() => *self.scroll_top = 0,
            // ESC ( <c> — designate G0.  `0` is DEC Special Graphics,
            // which is how a great many TUIs draw their boxes; every
            // other variant is one of the national ASCII sets, and
            // this terminal treats them all as ASCII.
            _ if intermediates == b"(" => *self.g0_graphics = byte == b'0',
            // DECALN (ESC # 8) — the screen alignment pattern: every
            // cell an uppercase E, default attributes, cursor home.
            // vttest opens with it, and a terminal that ignores it
            // shows the shell's own echo where the pattern should be,
            // which makes every test after it read against the wrong
            // screen.  DECALN also drops the scroll region, so the
            // pattern is not sliced by whatever was set before.
            b'8' if intermediates == b"#" => self.screen_alignment_pattern(),
            // RIS — what `reset` sends.
            b'c' if intermediates.is_empty() => self.hard_reset(),
            // Charset switching etc. arrive in later phases.
            _ => {}
        }
    }

    /// `CSI … m` as the slice path hands it over: what `csi_dispatch`
    /// does for that final byte, without the walk to it.  Colour
    /// changes are most of the control sequences coloured output is
    /// made of.
    fn sgr_dispatch(&mut self, params: &[u16], subs: &[bool]) {
        self.csi_prologue(params, &[], b'm');
        apply_sgr(self.attrs, params, subs);
    }

    fn csi_dispatch(&mut self, params: &[u16], subs: &[bool], intermediates: &[u8], byte: u8) {
        self.csi_prologue(params, intermediates, byte);
        // The DECAWM deferred wrap is cancelled per OPERATION, not by
        // every CSI that goes past.  Clearing it here meant an SGR
        // between the glyph that filled the last column and the next
        // glyph dropped the wrap, and that next glyph then overwrote
        // the last column instead of starting the next row — so a
        // coloured shell prompt wider than the pane lost its tail and
        // the cursor stuck at the right edge (2026-09-08 field report:
        // an oh-my-zsh prompt with a long branch name, `) ✗ ` gone).
        //
        // ghostty (references/ghostty, `src/terminal/Terminal.zig`)
        // clears in exactly these: print-that-wraps, carriageReturn,
        // cursorUp/Down/Right/Left, setCursorPos, index, restoreCursor,
        // insertLines, deleteLines, insertBlanks, eraseLine,
        // eraseDisplay — and explicitly PRESERVES it across scrollUp /
        // scrollDown, horizontalTab, deleteChars and eraseChars.  SGR,
        // mode set/reset, device queries and OSC never touch it.
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
                // `CSI ? u` — what kitty keyboard flags are in force?
                // The answer is what we actually honour, which is why
                // everything that writes the stack masks first: a
                // program encodes for what this reply says.
                b'u' => {
                    let flags = self.kitty_keyboard.current();
                    let mut buf = [0u8; 16];
                    let n = {
                        use std::io::Write;
                        let mut w = &mut buf[..];
                        let _ = write!(w, "\x1b[?{flags}u");
                        16 - w.len()
                    };
                    self.pending_response.extend_from_slice(&buf[..n]);
                    self.record_response("KITTY-KBD-QUERY");
                }
                // DECXCPR — `CSI ? 6 n`, the DEC-private twin of CPR.
                // Same reading of the cursor, `?`-prefixed, with the
                // page number xterm always reports as 1.  A program
                // that asks this way and gets nothing waits exactly as
                // long as one that asked with `CSI 6 n`.
                b'n' if params.first().copied() == Some(6) => {
                    let (c0, r0) = self.grid.cursor();
                    let (row, col) = (self.reported_row(r0), c0 + 1);
                    let mut buf = [0u8; 32];
                    let n = {
                        use std::io::Write;
                        let mut w = &mut buf[..];
                        let _ = write!(w, "\x1b[?{row};{col};1R");
                        32 - w.len()
                    };
                    self.pending_response.extend_from_slice(&buf[..n]);
                    self.record_response("DECXCPR");
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
            // Sequences that carry an intermediate have to be answered
            // HERE.  The arms further down are matched after this
            // early return, so anything written there for a `CSI <int>
            // ... <final>` is dead — XTQVERSION was, for as long as the
            // comment next to it claimed otherwise (measured 2026-09-07:
            // `CSI > 0 q` returned zero bytes).
            match (intermediates, byte) {
                // DECSCUSR — cursor shape.  0 and 1 are block, 3
                // underline, 5 bar; even values blink.  Recorded, not
                // yet drawn: measured on a 22 MB codex session, all
                // 119,812 of its DECSCUSR calls ask for shape 0, which
                // means "whatever this terminal's default is" — so
                // teaching the renderer the other shapes would change
                // nothing for the program that sends it most.
                (b" ", b'q') => *self.cursor_shape = param(params, 0, 0).min(6) as u8,
                // XTQVERSION — `CSI > 0 q`.  App wants the terminal's
                // name+version string.  Respond with a DCS reply:
                //   DCS > | marspot ESC \
                // Apps that recognise this fingerprint can tune their
                // behaviour; apps that don't ignore it.
                (b">", b'q') => {
                    self.pending_response
                        .extend_from_slice(b"\x1bP>|marspot\x1b\\");
                    self.record_response("XTQVERSION");
                }
                // DA2 (Secondary DA) — `CSI > c`.  App wants the
                // firmware version; xterm answers `CSI > 41;330;0 c`
                // (type 41 = VT420, version 330, ROM 0) and we mimic it.
                //
                // This arm used to sit below with the other `c` handling,
                // where the early return made it dead — exactly the way
                // XTQVERSION was dead, with a comment next to it
                // describing a reply it was not sending.  Measured
                // 2026-09-20: `CSI > c` returned zero bytes.  The note
                // above was already there; what was missing was a test
                // that could see it, which `probe_replies` now is.
                (b">", b'c') => {
                    self.pending_response.extend_from_slice(b"\x1b[>41;330;0c");
                    self.record_response("DA2");
                }
                // DECRQM — `CSI ? Ps $ p` asks whether a DEC private
                // mode is set; the answer is `CSI ? Ps ; Pm $ y`.
                //
                // Worth answering because we have something to say: a
                // program that cannot confirm ?2026 does not use
                // synchronized output, and marspot implements it — so
                // silence here costs a feature already built and paid
                // for.  Modes we don't implement report 0, which is the
                // spec's "never heard of it" and leaves the program on
                // its fallback.
                //
                // Deliberately NOT routed through `record_response`:
                // asking about several modes in a row at startup is what
                // a correct client does, and an instrument that warns on
                // correct behaviour is a broken instrument.
                // `CSI > flags u` — push.  `CSI < n u` — pop n (1 by
                // default).  `CSI = flags ; mode u` — set in place.
                // See `KittyKeyboardStack`; none of the three answers,
                // a program reads the result back with `CSI ? u`.
                (b">", b'u') => self.kitty_keyboard.push(param(params, 0, 0) as u8),
                (b"<", b'u') => self.kitty_keyboard.pop(param(params, 0, 1) as usize),
                (b"=", b'u') => {
                    let flags = param(params, 0, 0) as u8;
                    let mode = param(params, 1, 1);
                    self.kitty_keyboard.set(mode, flags);
                }
                // DECSTR — soft reset.  Modes back to a known state,
                // screen untouched.
                (b"!", b'p') => self.soft_reset(),
                (b"?$", b'p') => {
                    let mode = param(params, 0, 0);
                    let state = self.dec_mode_report(mode);
                    let mut buf = [0u8; 32];
                    let n = {
                        use std::io::Write;
                        let mut w = &mut buf[..];
                        let _ = write!(w, "\x1b[?{mode};{state}$y");
                        32 - w.len()
                    };
                    self.pending_response.extend_from_slice(&buf[..n]);
                    lx_debug!("term.respond", "DECRQM", mode = mode, state = state);
                }
                _ => {
                    lx_debug!(
                        "term.csi.unsupported",
                        "CSI with non-? intermediate",
                        final_byte = byte as char,
                        int_count = intermediates.len()
                    );
                }
            }
            return;
        }
        let (col, row) = self.grid.cursor();
        let cols = self.grid.cols();
        let rows = self.grid.rows();
        match byte {
            b'A' => {
                *self.pending_wrap = false;
                // CUU: cursor up by N (default 1), stopping at the top
                // margin -- a relative move does not step out of the
                // region the program set up.
                let n = param(params, 0, 1);
                let to = row.saturating_sub(n).max(self.up_limit(row));
                self.grid.set_cursor(col, to);
            }
            b'B' => {
                *self.pending_wrap = false;
                // CUD: cursor down by N, stopping at the bottom margin.
                // Without that, `CSI 24 B` inside a two-row region put
                // the cursor five rows below the region and the text
                // that followed was written outside it.
                let n = param(params, 0, 1);
                let to = row.saturating_add(n).min(self.down_limit(row));
                self.grid.set_cursor(col, to);
            }
            b'C' => {
                *self.pending_wrap = false;
                // CUF: cursor forward (right).
                let n = param(params, 0, 1);
                self.grid
                    .set_cursor(col.saturating_add(n).min(cols - 1), row);
            }
            b'E' => {
                *self.pending_wrap = false;
                // CNL: cursor next line — down N, column 0.  Same
                // margin as CUD; it is CUD with a carriage return.
                let n = param(params, 0, 1);
                let to = row.saturating_add(n).min(self.down_limit(row));
                self.grid.set_cursor(0, to);
            }
            b'F' => {
                *self.pending_wrap = false;
                // CPL: cursor previous line — up N, column 0.  Homebrew's
                // concurrent-download display redraws itself with
                // `\033[{n}F` (Tty.move_cursor_up_beginning); with this
                // missing the cursor never moved up and every refresh
                // APPENDED its lines — the 2026-07-28 "brew progress
                // scrolls forever" field report.
                //
                // Same margin as CUU; it is CUU with a carriage return.
                let n = param(params, 0, 1);
                let to = row.saturating_sub(n).max(self.up_limit(row));
                self.grid.set_cursor(0, to);
            }
            b'D' => {
                *self.pending_wrap = false;
                // CUB: cursor back (left).
                let n = param(params, 0, 1);
                self.grid.set_cursor(col.saturating_sub(n), row);
            }
            b'H' | b'f' => {
                *self.pending_wrap = false;
                // CUP / HVP: cursor position (1-indexed row;col → 0-indexed).
                let r = self.absolute_row(param(params, 0, 1).saturating_sub(1));
                let c = param(params, 1, 1).saturating_sub(1);
                self.grid.set_cursor(c, r);
            }
            b'G' => {
                *self.pending_wrap = false;
                // HPA: horizontal position absolute (1-indexed).
                let c = param(params, 0, 1).saturating_sub(1);
                self.grid.set_cursor(c, row);
            }
            b'I' => {
                // CHT — forward n tab stops.  Like HT, it does not
                // change rows, so the deferred wrap stands.
                let n = param(params, 0, 1).max(1);
                let to = self.tabs.next(col, n);
                self.grid.set_cursor(to, row);
            }
            b'Z' => {
                // CBT — backward n tab stops.
                *self.pending_wrap = false;
                let n = param(params, 0, 1).max(1);
                let to = self.tabs.prev(col, n);
                self.grid.set_cursor(to, row);
            }
            b'g' => {
                // TBC — clear this column's stop (0, the default) or
                // every stop (3).  Other values are undefined and
                // ignored rather than guessed at.
                match param(params, 0, 0) {
                    0 => self.tabs.clear(col),
                    3 => self.tabs.clear_all(),
                    _ => {}
                }
            }
            b'd' => {
                *self.pending_wrap = false;
                // VPA: vertical position absolute (1-indexed), inside
                // the origin like CUP.
                let r = self.absolute_row(param(params, 0, 1).saturating_sub(1));
                self.grid.set_cursor(col, r);
            }
            b'J' => {
                *self.pending_wrap = false;
                // ED: erase in display.  Cursor is not moved.
                // 0 (default) — from cursor (inclusive) to end of screen
                // 1           — from start of screen to cursor (inclusive)
                // 2           — entire screen
                // 3           — entire scrollback (xterm extension)
                let mode = param_raw(params, 0, 0);
                lx_debug_sampled!(
                    "term.edit.ED",
                    4,
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
                *self.pending_wrap = false;
                // EL: erase in line.  Cursor not moved.
                // 0 (default) — from cursor (inclusive) to end of line
                // 1           — from start of line to cursor (inclusive)
                // 2           — entire line
                let mode = param_raw(params, 0, 0);
                lx_debug_sampled!(
                    "term.edit.EL",
                    4,
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
                apply_sgr(self.attrs, params, subs);
            }
            // SCO save / restore cursor.  Same slot, same contents as
            // DECSC and DECRC.
            b's' => self.save_cursor(),
            b'u' => self.restore_cursor(),
            // SM / RM without the `?`: the ANSI modes.  IRM is the one
            // that does anything here; the rest of the set is either
            // about the keyboard (KAM), the host echo (SRM) or a
            // newline convention (LNM) that no program on a pty relies
            // on this terminal for.
            b'h' | b'l' => {
                let set = byte == b'h';
                for &p in params {
                    if p == 4 {
                        *self.insert_mode = set;
                    } else {
                        lx_debug!("term.mode.ansi", "ANSI mode ignored", mode = p, set = set);
                    }
                }
            }
            b'r' => {
                *self.pending_wrap = false;
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
                // Home, which under DECOM is the region's first row.
                self.cursor_to_origin();
            }
            b'L' => {
                *self.pending_wrap = false;
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
                    self.grid
                        .scroll_down_region(row, *self.scroll_bot, n, blank_with(*self.attrs));
                    self.grid.set_cursor(0, row);
                }
            }
            b'M' => {
                *self.pending_wrap = false;
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
                    self.grid
                        .scroll_up_region(row, *self.scroll_bot, n, blank_with(*self.attrs));
                    self.grid.set_cursor(0, row);
                }
            }
            b'@' => {
                *self.pending_wrap = false;
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
            // MC — media copy.  There is no printer: every form is
            // accepted and ignored, and what a program meant for the
            // printer stays on the screen.
            b'i' => {}
            b'c' if intermediates.is_empty() => {
                // DA — Primary Device Attributes; see `DA1_REPLY`.
                self.pending_response.extend_from_slice(DA1_REPLY);
                self.record_response("DA1");
            }
            b't' if intermediates.is_empty() => {
                // XTWINOPS.  Two kinds of thing share this final byte:
                // questions about the window, and orders to move,
                // resize, raise or iconify it.  Only one question is
                // answered and every order is refused.
                match params.first().copied().unwrap_or(0) {
                    // 18 — how big is the text area, in characters?
                    // `CSI 8 ; rows ; cols t`.  We know this exactly,
                    // and a program that asks and hears nothing goes
                    // on using whatever it last assumed.
                    18 => {
                        let mut buf = [0u8; 32];
                        let n = {
                            use std::io::Write;
                            let mut w = &mut buf[..];
                            let _ =
                                write!(w, "\x1b[8;{};{}t", self.grid.rows(), self.grid.cols());
                            32 - w.len()
                        };
                        self.pending_response.extend_from_slice(&buf[..n]);
                        self.record_response("XTWINOPS-SIZE");
                    }
                    // 14 / 16 — the same area and one cell, in PIXELS.
                    // Refused rather than unimplemented: the numbers
                    // live in the renderer, two processes away from
                    // here, and the only reason to ask is to place an
                    // image — which marspot does not display.  Wiring
                    // a cross-process pixel feed for a question whose
                    // answer has no use would be building the harder
                    // half of a feature that does not exist.
                    //
                    // 1-13, 15, 17, 19-24 — raise, lower, move,
                    // resize, iconify, and read back the title.  A
                    // program does not get to move this window or to
                    // read text out of it; xterm disables most of
                    // these by default for the same reason.
                    _ => {
                        lx_debug!(
                            "term.csi.unsupported",
                            "XTWINOPS declined",
                            param0 = params.first().copied().unwrap_or(0)
                        );
                    }
                }
            }
            b'n' if intermediates.is_empty() => {
                // DSR — Device Status Report.  `CSI 5 n` asks whether
                // the terminal is alive; `CSI 6 n` (CPR) asks where the
                // cursor is and expects `CSI <row> ; <col> R`, 1-based,
                // in the CURRENT origin.
                //
                // Not an exotic sequence: readline redraws a wrapped
                // prompt by asking for the column, and a program that
                // gets no answer waits for one — the reason this was
                // worth adding is that a terminal which never replies
                // makes its own throughput unmeasurable from outside
                // (a writer cannot tell "consumed" from "dropped"), and
                // it hangs any app that asks.
                match params.first().copied().unwrap_or(0) {
                    5 => {
                        self.pending_response.extend_from_slice(b"\x1b[0n");
                        self.record_response("DSR-status");
                    }
                    6 => {
                        // `Grid::cursor` is (col, row), 0-based; CPR is
                        // (row, col), 1-based, and counted from the
                        // same origin the program addresses with -- a
                        // report in screen rows to a program working in
                        // region rows sends it to the wrong line.
                        let (c0, r0) = self.grid.cursor();
                        let (row, col) = (self.reported_row(r0), c0 + 1);
                        let mut buf = [0u8; 24];
                        let n = {
                            use std::io::Write;
                            let mut w = &mut buf[..];
                            let _ = write!(w, "\x1b[{row};{col}R");
                            24 - w.len()
                        };
                        self.pending_response.extend_from_slice(&buf[..n]);
                        self.record_response("DSR-CPR");
                    }
                    _ => {}
                }
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
        self.osc(data);
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod osc_tests;

#[cfg(test)]
mod sync_output_tests;

#[cfg(test)]
mod alt_scroll_tests;

#[cfg(test)]
mod alt_screen_across_execv_tests;

#[cfg(test)]
mod u_tag_tests;

#[cfg(test)]
mod attrs_handover_tests;

#[cfg(test)]
mod predict_learning_tests;

#[cfg(test)]
mod predict_edge_tests;
