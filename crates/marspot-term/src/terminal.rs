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

use crate::grid::{Cell, CellAttrs, Color, DEFAULT_SCROLLBACK_LINES, Grid};
use crate::parser::{Parser, ParserCallbacks};
use crate::scrollback::Scrollback;
use crate::{lx_debug, lx_debug_sampled, lx_info, lx_warn};
use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

/// F2 — file-backed scrollback is on whenever a session id is present
/// (L3 `marspot-session` process).  mcli / `--snapshot` / tests don't
/// set `MARSPOT_SESSION_ID` and so fall through to in-RAM Memory.  The
/// old `MARSPOT_FILE_SCROLLBACK` / `MARSPOT_DISK_SCROLLBACK` opt-out
/// gates were removed after F1 soaked; both were no-ops for the live
/// app and only existed for bisect rollback.
fn file_scrollback_session_id() -> Option<u64> {
    // Unit tests must never bind to a real session's scrollback
    // file.  The var leaks into test processes when the suite runs
    // inside a marspot terminal (L3 exports it to its shell, every
    // descendant inherits) — with it set, each `Terminal::new` in a
    // test opened sessions/<id>/scrollback.bin in the REAL state dir
    // and overwrote the pane's on-disk history.  Under `cfg!(test)`
    // the File variant therefore also requires an explicit
    // MARSPOT_STATE_DIR (the A3/A4 e2e tests set a per-test sandbox
    // dir; a leaked production env never has it).  bin/test.sh +
    // bin/bench.sh additionally unset the var for out-of-crate
    // consumers, where cfg!(test) is false.
    if cfg!(test) && std::env::var_os("MARSPOT_STATE_DIR").is_none() {
        return None;
    }
    std::env::var("MARSPOT_SESSION_ID")
        .ok()?
        .parse::<u64>()
        .ok()
}

/// In-RAM ring size used as the front-line cache by `FileScrollback`.
/// Sized at 256 lines: each session sits on ~256 × cols × 24 B ≈ 750 KiB
/// of RAM ring; at 9 panes that's ~7 MiB total — a fraction of the
/// 1024-line predecessor.  Larger rings faulted into the parse hot
/// path (4096-line variant cost ~3% cat-ascii throughput); smaller
/// rings lost the recent-line fast path for search worker /
/// mid-burst scrollback reads.  Data on
/// `feature/disk-scrollback-mmap` 2026-05-04.
const FILE_SCROLLBACK_RAM_LINES: usize = 256;

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
    /// it today (see `osc_dispatch`).
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
        // F2 — file-backed scrollback wins inside an L3 session
        // (MARSPOT_SESSION_ID set).  Corrupt pairs are quarantined +
        // recreated inside `Scrollback::file` (RFC-004 A.4), so the
        // only errors reaching here are environmental (permissions,
        // ENOSPC) — those fall back to in-RAM Memory so the session
        // still boots, and the failure lands in marspot.log (the old
        // eprintln went to /dev/null: L3's stderr is nulled).
        // Tests / mcli / --snapshot don't set the env and so go
        // straight to Memory.
        let scrollback = if let Some(sid) = file_scrollback_session_id() {
            match crate::scrollback::Scrollback::file(
                crate::session_registry::scrollback_bin_path(sid),
                crate::session_registry::scrollback_idx_path(sid),
                cols as usize,
                FILE_SCROLLBACK_RAM_LINES,
            ) {
                Ok(sb) => sb,
                Err(e) => {
                    crate::lx_warn!(
                        "scrollback.open_failed_ram_fallback",
                        &format!("{e} — session runs RAM-only, disk history not loaded"),
                        session_id = sid
                    );
                    Scrollback::memory(DEFAULT_SCROLLBACK_LINES, cols as usize)
                }
            }
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
            render_markup: false,
            u_tags: false,
            u_open: false,
            u_buf: String::new(),
            alt_scroll: false,
            sync_output: false,
            focus_reporting: false,
            focus_reported: None,
            uses_sync_output: false,
            osc_title: String::new(),
            cursor_shape: 0,
            cursor_visible: true,
            mouse_tracking_mode: MouseTrackingMode::Off,
            mouse_sgr_encoding: false,
            pending_wrap: false,
            predict_misses: 0,
            predict_declined: 0,
            predictions: VecDeque::new(),
            shadow: VecDeque::new(),
            echo_srtt: Self::ECHO_SRTT_SEED,
            cluster_buf: String::new(),
            cluster_fast: None,
            cluster_anchor: None,
            grapheme_cursor: crate::grapheme::GraphemeCursor::new(),
            seg_synced: true,
            predictions_hit: 0,
            predictions_miss: 0,
            predictions_expired: 0,
            generation: 0,
        }
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
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
        self.bracketed_paste_mode
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
        self.bracketed_paste_mode = false;
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
        // of mine: 300 KB of output, not one reset in it).
        self.attrs = CellAttrs::default();
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

    /// Three wasted keystrokes is a small, bounded amount of flicker
    /// to pay to find out that a program does not echo.
    const PREDICT_GIVE_UP_AFTER: u32 = 3;
    /// And one every so often to find out whether it has changed —
    /// the pane a program was quit in becomes a shell again, and
    /// nothing else announces that.  Rare enough that the stray
    /// character is not the thing anyone notices.
    const PREDICT_PROBE_EVERY: u32 = 128;

    /// Starting guess for the echo round trip.  Measured 2026-09-07
    /// against a real zsh on a pty: idle p50 0.16 ms / max 1.12 ms,
    /// and 0.16 ms / 8.18 ms with the shell flooding the pipe at the
    /// same time.  10 ms is comfortably above all of it and is only
    /// a seed — `echo_srtt` moves to whatever the link actually is.
    const ECHO_SRTT_SEED: Duration = Duration::from_millis(10);
    /// A prediction is abandoned after this long unanswered.  Four
    /// round trips, floored well above local pty latency and capped
    /// so that even a very slow link cannot leave a character from
    /// an unechoed password sitting on screen for a whole second.
    const PREDICT_DEADLINE_MIN: Duration = Duration::from_millis(50);
    const PREDICT_DEADLINE_MAX: Duration = Duration::from_millis(1000);

    /// How long an unanswered prediction is given before it is taken
    /// back off the screen.
    pub fn predict_deadline(&self) -> Duration {
        (self.echo_srtt * 4).clamp(Self::PREDICT_DEADLINE_MIN, Self::PREDICT_DEADLINE_MAX)
    }

    /// True while at least one guess is still on screen unconfirmed —
    /// the caller's cue to come back and check the deadline rather
    /// than sleep until the next byte, which may never come.
    pub fn predictions_pending(&self) -> bool {
        !self.predictions.is_empty()
    }

    /// Take back every prediction that has gone unanswered past the
    /// deadline.  Their bytes move to the shadow queue: whether the
    /// program was slow or is simply never going to echo is decided
    /// by what arrives next, not here.  Returns true iff the grid
    /// changed (caller should redraw).
    pub fn expire_predictions_at(&mut self, now: Instant) -> bool {
        let deadline = self.predict_deadline();
        match self.predictions.front() {
            Some(p) if now.duration_since(p.at) >= deadline => {}
            _ => return false,
        }
        const SHADOW_MAX: usize = 64;
        for p in &self.predictions {
            if self.shadow.len() >= SHADOW_MAX {
                self.shadow.pop_front();
            }
            self.shadow.push_back((p.byte, p.at));
        }
        self.predictions_expired += self.predictions.len() as u64;
        self.rollback_predictions_inner(false);
        true
    }

    /// Wall-clock wrapper over [`Self::expire_predictions_at`].
    pub fn expire_predictions(&mut self) -> bool {
        self.expire_predictions_at(Instant::now())
    }

    /// Fold one observed keystroke-to-echo round trip into the
    /// smoothed estimate (the usual 7/8 EWMA).
    fn note_echo_rtt(&mut self, sample: Duration) {
        self.echo_srtt = (self.echo_srtt * 7 + sample) / 8;
    }

    /// True iff local-echo prediction is currently worth making.
    ///
    /// Alt screen is refused outright: vim / less / htop do not echo
    /// keys verbatim.  Beyond that the terminal does not guess what
    /// KIND of program is on the other end — it reads what happened
    /// to the last few guesses.  termios cannot answer this: a zsh
    /// prompt is `-icanon -echo` exactly like codex, because ZLE
    /// draws its own line too.  The difference is only WHERE, and the
    /// tally is the only thing that sees it.
    pub fn can_predict(&self) -> bool {
        self.saved_main.is_none()
            && (self.predict_misses < Self::PREDICT_GIVE_UP_AFTER
                || self.predict_declined >= Self::PREDICT_PROBE_EVERY)
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
            self.predict_declined = self.predict_declined.saturating_add(1);
            return false;
        }
        // Taking the probe: whatever it teaches, it is spent.
        self.predict_declined = 0;
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
        self.grid.set_cell(
            col,
            row,
            Cell {
                ch: byte as char,
                attrs: self.attrs,
            },
        );
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
            at: Instant::now(),
        });
        true
    }

    /// Reverse-pop every pending prediction, restoring its saved
    /// cell + cursor.  After this, the grid is back to whatever it
    /// looked like before the first un-confirmed prediction.
    fn rollback_predictions(&mut self) {
        self.rollback_predictions_inner(true);
    }

    /// The unpainting itself.  `count_miss` separates "the program
    /// echoed something else" (a miss) from "nothing came back yet"
    /// (an expiry) — same pixels restored, different verdict.
    fn rollback_predictions_inner(&mut self, count_miss: bool) {
        while let Some(p) = self.predictions.pop_back() {
            self.grid
                .set_cell(p.saved_cursor.0, p.saved_cursor.1, p.saved_cell);
            self.grid.set_cursor(p.saved_cursor.0, p.saved_cursor.1);
            if count_miss {
                self.predictions_miss += 1;
            }
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
        // Once per chunk, not once per character.  The pane's own
        // declaration is the normal route; the setting is a manual
        // override for someone who wants it everywhere.
        self.u_tags = self.render_markup || crate::settings::get().render_u_tags;
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
            // Bulk program output has neither queue populated — the
            // overwhelmingly common case on this per-byte path, and
            // one test is what it costs.
            if !(self.predictions.is_empty() && self.shadow.is_empty()) {
                // A byte owed to an expired prediction settles the
                // question that expiry deliberately left open.  Matching
                // means the program does echo, just later than we waited
                // — widen the window and keep predicting.  Anything else
                // means it was never going to, which is a real miss.  The
                // byte itself is fed normally either way: its cell was
                // unpainted when the prediction expired.
                let mut owed = false;
                if let Some(&(b, at)) = self.shadow.front() {
                    if bytes[i] == b {
                        self.shadow.pop_front();
                        self.note_echo_rtt(Instant::now().duration_since(at));
                        self.predict_misses = 0;
                        owed = true;
                    } else {
                        self.predict_misses = self.predict_misses.saturating_add(1);
                        self.shadow.clear();
                    }
                }
                // Validate against pending predictions before the parser
                // sees the byte.
                if let Some(p) = self.predictions.front() {
                    if !owed && bytes[i] == p.byte {
                        let at = p.at;
                        self.predictions.pop_front();
                        self.predictions_hit += 1;
                        self.note_echo_rtt(Instant::now().duration_since(at));
                        // The program echoes at the cursor after all.
                        self.predict_misses = 0;
                        i += 1;
                        continue;
                    }
                    if !owed {
                        self.predict_misses = self.predict_misses.saturating_add(1);
                        self.rollback_predictions();
                    }
                }
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
            let sync_output = &mut self.sync_output;
            let uses_sync_output = &mut self.uses_sync_output;
            let focus_reporting = &mut self.focus_reporting;
            let focus_reported = &mut self.focus_reported;
            let osc_title = &mut self.osc_title;
            let cursor_shape = &mut self.cursor_shape;
            let alt_scroll = &mut self.alt_scroll;
            let u_tags = self.u_tags;
            let u_buf = &mut self.u_buf;
            let u_open = &mut self.u_open;
            let cursor_visible = &mut self.cursor_visible;
            let mouse_tracking_mode = &mut self.mouse_tracking_mode;
            let mouse_sgr_encoding = &mut self.mouse_sgr_encoding;
            let pending_wrap = &mut self.pending_wrap;
            let cluster_buf = &mut self.cluster_buf;
            let cluster_fast = &mut self.cluster_fast;
            let cluster_anchor = &mut self.cluster_anchor;
            let grapheme_cursor = &mut self.grapheme_cursor;
            let seg_synced = &mut self.seg_synced;
            let mut handler = Handler {
                grid,
                saved_main,
                attrs,
                saved_cursor,
                scroll_top,
                scroll_bot,
                pending_response,
                response_window,
                response_burst_last_warn,
                cursor_key_app_mode,
                bracketed_paste,
                sync_output,
                uses_sync_output,
                focus_reporting,
                focus_reported,
                osc_title,
                cursor_shape,
                alt_scroll,
                u_tags,
                u_buf,
                u_open,
                cursor_visible,
                mouse_tracking_mode,
                mouse_sgr_encoding,
                pending_wrap,
                cluster_buf,
                cluster_fast,
                cluster_anchor,
                grapheme_cursor,
                seg_synced,
            };
            // BATCH LANES — parser in plain Ground: hand whole
            // printable runs to the handler in one call, skipping the
            // per-byte state machine (a printable run can't change
            // parser state) and the per-char cursor round-trips.
            // Predictions must be empty — the per-byte loop is what
            // validates them.
            if parser.in_ground_plain() && self.predictions.is_empty() {
                // Which lane can possibly apply is decided by the
                // first byte — all three scans below start by
                // rejecting anything outside their own leading-byte
                // range, so running them in sequence asked the same
                // question three times per character.  Emoji prose
                // alternates 4-byte scalars with single spaces, i.e.
                // it takes this path twice per glyph.
                let b0 = bytes[i];
                if (0x20..=0x7E).contains(&b0) {
                    // ASCII lane: longest 0x20..=0x7E run.
                    let run_len = bytes[i..]
                        .iter()
                        .position(|&b| !(0x20..=0x7E).contains(&b))
                        .unwrap_or(bytes.len() - i);
                    if run_len >= 2 {
                        handler.print_ascii_run(&bytes[i..i + run_len]);
                        i += run_len;
                        continue;
                    }
                    // A lone printable — the shape emoji prose takes, where
                    // every glyph is separated by exactly one space, so the
                    // run lane never applies and each space fell through to
                    // the per-byte state machine.  In plain Ground with no
                    // predictions pending, a byte in 0x20..=0x7E dispatches
                    // to `print` and nothing else, so calling it directly
                    // is the same work minus the dispatch.
                    if run_len == 1 {
                        handler.print(b0 as char);
                        i += 1;
                        continue;
                    }
                } else if (0xE0..=0xEF).contains(&b0) {
                    // Wide lane: longest run of 3-byte UTF-8 sequences
                    // decoding to boring width-2 chars (CJK / kana /
                    // fullwidth).  Scan and commit each decode once —
                    // three shifts, far cheaper than the 3× per-byte
                    // state-machine dispatch they replace.
                    let wide_len = wide_boring_run_len(&bytes[i..]);
                    if wide_len >= 6 {
                        handler.print_wide_run(&bytes[i..i + wide_len]);
                        i += wide_len;
                        continue;
                    }
                } else if (0xF0..=0xF4).contains(&b0) {
                    // Decode lane: a run of structurally-valid 4-byte
                    // UTF-8 sequences (emoji plane).  Unlike the lanes
                    // above this commits NOTHING early — `print` sees
                    // the exact same codepoint stream the per-byte state
                    // machine would deliver (including the same
                    // replacement-char behaviour), so cluster semantics
                    // are untouched; only the 4× per-byte dispatch is
                    // skipped.
                    // Threshold is one sequence, not two.  Emoji prose is
                    // `🚀 ✨ 🎉` — a 4-byte scalar between single spaces,
                    // so a run of two never occurs and this lane sat idle
                    // on the one workload it was built for, leaving four
                    // per-byte state-machine dispatches per glyph (37 % of
                    // parse time, 2026-08-18 sample).  One sequence is
                    // already worth decoding directly: the lane commits
                    // nothing early, so the trade is 4 dispatches for 1
                    // decode with the codepoint stream unchanged.
                    let quad_len = quad_run_len(&bytes[i..]);
                    if quad_len >= 4 {
                        for chunk in bytes[i..i + quad_len].chunks_exact(4) {
                            let cp = (((chunk[0] & 0x07) as u32) << 18)
                                | (((chunk[1] & 0x3F) as u32) << 12)
                                | (((chunk[2] & 0x3F) as u32) << 6)
                                | (chunk[3] & 0x3F) as u32;
                            match char::from_u32(cp) {
                                Some(c) => handler.print(c),
                                None => handler.print(crate::parser::REPLACEMENT_CHAR),
                            }
                        }
                        i += quad_len;
                        continue;
                    }
                }
            }
            parser.advance(&mut handler, bytes[i]);
            i += 1;

            // Entering alt-screen mid-feed swaps the grid wholesale —
            // any predictions queued beforehand were anchored to the
            // old grid and are now garbage.  Drop them.
            if !self.predictions.is_empty() && self.saved_main.is_some() {
                let n = self.predictions.len();
                self.predictions.clear();
                self.shadow.clear();
                self.predictions_miss += n as u64;
            }
        }
        // NOTHING is flushed at the end of a feed any more, and that
        // is the point.
        //
        // A glyph is committed the moment its width is known, so there
        // has never been anything pending here since that change — the
        // keystroke has already landed.  What this block used to do
        // besides write was CLEAR the open cluster, and that is what
        // made a cluster split across two reads come apart: the base
        // was drawn and the cursor moved, so the codepoint that would
        // have extended it took a cell of its own.  The comment here
        // used to claim the segmenter's saved state resolved that; it
        // did not, and a pty ends its reads wherever the kernel had a
        // break, so it was not rare either (measured 2026-09-07:
        // `a⚠️b` split after `⚠`).
        //
        // The cluster and its anchor now survive the boundary, which
        // is what lets the next read finish the cluster.
        //
        // Build a one-off handler so the flush method can do its work
        // through the same borrows as the per-byte handler.
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
        let mut modes: u32 = 0;
        if self.cursor_key_application_mode {
            modes |= 1 << 0;
        }
        if self.bracketed_paste_mode {
            modes |= 1 << 1;
        }
        if self.cursor_visible {
            modes |= 1 << 2;
        }
        if self.pending_wrap {
            modes |= 1 << 3;
        }
        if self.saved_main.is_some() {
            modes |= 1 << 4;
        }
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
        if snapshot_v < SNAPSHOT_MIN_COMPAT || snapshot_v > SNAPSHOT_VERSION {
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
            })
        } else {
            None
        };
        let want_cells = cols as usize * rows as usize;
        let mut cells = Vec::with_capacity(want_cells);
        for _ in 0..want_cells {
            let ch_u = read_u32(&mut cur)?;
            let cell_attrs = read_attrs(&mut cur)?;
            let ch = char::from_u32(ch_u).unwrap_or(' ');
            cells.push(Cell {
                ch,
                attrs: cell_attrs,
            });
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
                    let ch_u = read_u32(&mut cur)?;
                    let cell_attrs = read_attrs(&mut cur)?;
                    let ch = char::from_u32(ch_u).unwrap_or(' ');
                    line.push(Cell {
                        ch,
                        attrs: cell_attrs,
                    });
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
                        let ch_u = read_u32(&mut cur)?;
                        let cell_attrs = read_attrs(&mut cur)?;
                        line.push(Cell {
                            ch: char::from_u32(ch_u).unwrap_or(' '),
                            attrs: cell_attrs,
                        });
                    }
                    ring.push((line, wrapped));
                }
                let want = a_cols as usize * a_rows as usize;
                let mut cells = Vec::with_capacity(want);
                for _ in 0..want {
                    let ch_u = read_u32(&mut cur)?;
                    let cell_attrs = read_attrs(&mut cur)?;
                    cells.push(Cell {
                        ch: char::from_u32(ch_u).unwrap_or(' '),
                        attrs: cell_attrs,
                    });
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
                        let ch_u = read_u32(&mut cur)?;
                        let cell_attrs = read_attrs(&mut cur)?;
                        let ch = char::from_u32(ch_u).unwrap_or(' ');
                        line.push(Cell {
                            ch,
                            attrs: cell_attrs,
                        });
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
        self.bracketed_paste_mode = (modes & (1 << 1)) != 0;
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
        // Bit 4 (in_alt_screen) is informational for the wire format
        // but not actionable here — apply_snapshot replaces the
        // current grid; alt-mode save state is regenerated on the
        // next `?1049h` toggle from the PTY stream.  The EXECV path
        // does not rely on it: `serialize_snapshot_live` carries the
        // alt screen verbatim and rebuilds `saved_main` underneath.
        self.attrs = attrs;
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
        // Replay scrollback in arrival order so the ring rebuilds
        // exactly the same shape it had pre-execv.  Wrapped flag is
        // carried across so cross-row URL / path link scans on
        // historic content keep working after execv.
        //
        // v3 path (correct by construction): the snapshot tells us
        // each line's logical index — `start_logical_idx + i` is the
        // line index of `scrollback_lines[i]`.  We skip indices the
        // on-disk scrollback already has and push only the suffix.
        // This dedups against the persistent file AND refills the
        // BufWriter-tail gap an L3 self-execv used to lose under the
        // old F2+4 skip (`project_scrollback_execv_gap`).
        //
        // v2 path (legacy fallback): no per-line index → fall back
        // to the F2+4 heuristic "skip replay if disk has anything".
        // Loses the BufWriter tail same as before, but v2 senders
        // only exist on pre-rollout images, so this is a transitional
        // path that disappears once v3 is the floor.
        let on_disk = self.grid.scrollback_len() as u64;
        if snapshot_v >= 3 {
            let start_idx = snapshot_start_logical_idx;
            let skip = on_disk.saturating_sub(start_idx) as usize;
            for (line, wrapped) in scrollback_lines.into_iter().skip(skip) {
                self.grid.push_historic_scrollback_line(&line, wrapped);
            }
        } else if on_disk == 0 {
            for (line, wrapped) in scrollback_lines {
                self.grid.push_historic_scrollback_line(&line, wrapped);
            }
        }
        // v4 alt fold — the alt grid's ring + final screen existed
        // only in RAM (B13), so there's nothing to dedup against:
        // append into scrollback, which (File-backed) also persists
        // them for every future boot.  A resurrected claudecode pane
        // thus keeps its conversation as scrollable history above
        // the restored main screen.
        for (line, wrapped) in alt_fold_lines {
            self.grid.push_historic_scrollback_line(&line, wrapped);
        }
        // v5 alt LIVE — rebuild the alt screen exactly as it was at
        // execv time: main grid (just restored above) moves into
        // saved_main, a fresh Memory-backed alt grid takes over with
        // the serialized ring + cells + cursor + scroll region.  The
        // still-running TUI's incremental repaints then land on the
        // same base they left.
        if let Some(live) = alt_live {
            let mut alt = Grid::with_scrollback(live.cols, live.rows, DEFAULT_SCROLLBACK_LINES);
            for (line, wrapped) in &live.ring {
                alt.push_historic_scrollback_line(line, *wrapped);
            }
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
            });
            self.scroll_top = live.scroll_top;
            self.scroll_bot = live.scroll_bot;
        }
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
    ///   [line_cols u32 LE]                — width of THIS line
    ///   [wrapped  u8]                     — 1 if this row is a
    ///                                       continuation of the previous
    ///                                       row (autowrap), 0 if it
    ///                                       started a fresh logical line
    ///   [cells: line_cols × 13 bytes]     — same Cell encoding as
    ///                                       `serialize_snapshot`
    /// ```
    ///
    /// Without the `wrapped` flag the receiver's later `Grid::resize`
    /// reflow can't tell hard newlines from autowrap continuations,
    /// and long lines stay chopped at the narrowest width the grid
    /// ever saw.
    pub fn serialize_scrollback_page(&self, line_start: u32, count: u32) -> (u32, Vec<u8>) {
        let lines = self
            .grid
            .scrollback_read_page(line_start as usize, count as usize);
        let line_count = lines.len() as u32;
        let body_bytes: usize = lines
            .iter()
            .map(|(l, _)| 4 + 1 + l.len() * CELL_BYTES)
            .sum();
        let mut out = Vec::with_capacity(body_bytes);
        for (line, wrapped) in &lines {
            out.extend_from_slice(&(line.len() as u32).to_le_bytes());
            out.push(if *wrapped { 1 } else { 0 });
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
    /// live data has scrolled off the visible grid.  `wrapped` is
    /// the line's autowrap-continuation flag from L4 (see the
    /// wire-format doc on `serialize_scrollback_page`).
    pub fn push_historic_line(&mut self, line: &[Cell], wrapped: bool) {
        self.grid.push_historic_scrollback_line(line, wrapped);
    }

    /// Inverse of `serialize_scrollback_page`.  Static — no `&self`
    /// because the decoder doesn't touch terminal state; callers
    /// (L3 publish-cache) decide what to do with the lines.
    ///
    /// Wire-compat: tries the current "with-wrapped" layout first
    /// (each line = `[cols u32][wrapped u8][cells]`).  If it doesn't
    /// consume exactly `body.len()` bytes, falls back to the legacy
    /// layout (`[cols u32][cells]`, no wrapped byte) emitted by L4
    /// shelld 0.2.6 and earlier — in that case every row is reported
    /// `wrapped = false`, so resize-time reflow won't glue continuation
    /// rows but the cell contents are correct.  Once every running
    /// L4 has been upgraded past 0.2.7 the legacy branch can be
    /// deleted.
    pub fn decode_scrollback_page_body(
        line_count: u32,
        body: &[u8],
    ) -> io::Result<Vec<(Vec<Cell>, bool)>> {
        if let Ok((out, consumed)) = decode_with_wrapped(line_count, body) {
            if consumed == body.len() {
                return Ok(out);
            }
        }
        decode_legacy_no_wrapped(line_count, body)
    }
}

fn decode_with_wrapped(
    line_count: u32,
    body: &[u8],
) -> io::Result<(Vec<(Vec<Cell>, bool)>, usize)> {
    let mut cur = Cursor::new(body);
    let mut out = Vec::with_capacity(line_count as usize);
    for _ in 0..line_count {
        let line_cols = read_u32(&mut cur)? as usize;
        let wrapped = read_u8(&mut cur)? != 0;
        let mut line = Vec::with_capacity(line_cols);
        for _ in 0..line_cols {
            let ch_u = read_u32(&mut cur)?;
            let attrs = read_attrs(&mut cur)?;
            let ch = char::from_u32(ch_u).unwrap_or(' ');
            line.push(Cell { ch, attrs });
        }
        out.push((line, wrapped));
    }
    Ok((out, cur.position() as usize))
}

fn decode_legacy_no_wrapped(line_count: u32, body: &[u8]) -> io::Result<Vec<(Vec<Cell>, bool)>> {
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
        out.push((line, false));
    }
    Ok(out)
}

// ─── Snapshot wire format helpers ─────────────────────────────────────

use std::io::Cursor;

const SNAPSHOT_MAGIC: u32 = 0xA557_5301;
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
const SNAPSHOT_VERSION: u32 = 5;
const SNAPSHOT_MIN_COMPAT: u32 = 1;
const ALT_SECTION_NONE: u8 = 0;
const ALT_SECTION_FOLD: u8 = 1;
const ALT_SECTION_LIVE: u8 = 2;
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
const SNAPSHOT_SCROLLBACK_LINE_CAP: usize = 20_000;
const ATTRS_BYTES: usize = 9;
const CELL_BYTES: usize = 4 + ATTRS_BYTES;

/// Public re-exports for sibling modules (`scrollback::FileScrollback`)
/// that need to serialise / deserialise cells with the same byte
/// layout snapshot uses.  Keeping the constants private and exposing
/// only `*_PUB` aliases avoids accidental external dependency on the
/// numeric values — they're an internal ABI gated by the
/// `cell_abi` field in `scrollback.bin`'s header.
pub const CELL_BYTES_PUB: usize = CELL_BYTES;
pub const ATTRS_BYTES_PUB: usize = ATTRS_BYTES;

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
        fg: decode_color(fg_kind, fg_payload).unwrap_or(Color::Default),
        bg: decode_color(bg_kind, bg_payload).unwrap_or(Color::Default),
    }
}

fn serialize_attrs(a: CellAttrs) -> [u8; ATTRS_BYTES] {
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
        bold: (flags & (1 << 0)) != 0,
        italic: (flags & (1 << 1)) != 0,
        underline: (flags & (1 << 2)) != 0,
        reverse: (flags & (1 << 3)) != 0,
        dim: (flags & (1 << 4)) != 0,
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
    mouse_tracking_mode: &'a mut MouseTrackingMode,
    mouse_sgr_encoding: &'a mut bool,
    pending_wrap: &'a mut bool,
    cluster_buf: &'a mut String,
    /// See `Terminal::cluster_fast`.
    cluster_fast: &'a mut Option<(char, u8)>,
    /// See `Terminal::cluster_anchor`.
    cluster_anchor: &'a mut Option<(u16, u16, u8)>,
    grapheme_cursor: &'a mut crate::grapheme::GraphemeCursor,
    seg_synced: &'a mut bool,
}

/// The fast-path class for `Handler::print`: codepoints that can
/// never open, extend, or join a multi-codepoint cluster — GBP=Other,
/// not Extended_Pictographic, InCB=None — with their cell width.  Two
/// neighbours from this class always have a cluster boundary between
/// them (UAX #29 GB999) and their width needs no table walk, so the
/// segmenter can be skipped entirely.
///
/// The ranges are hand-picked hot blocks (ASCII, CJK ideographs,
/// kana, CJK punctuation, fullwidth forms); every member is verified
/// against the real UCD tables by `fast_path_class_is_sound`, so a
/// table regen that invalidated one would fail the suite before it
/// could mis-render.  Deliberately NOT in the class: Hangul
/// syllables (GBP LV/LVT, GB6-8), anything Extended_Pictographic
/// (a following VS16/ZWJ changes width — early commit would be
/// wrong), combining kana marks U+3099/309A (GBP Extend).
/// Length (in bytes, multiple of 3) of the longest prefix of `bytes`
/// consisting of 3-byte UTF-8 sequences that decode to `boring_width
/// == Some(2)` chars.  The feed loop's wide batch lane.  Overlong /
/// surrogate encodings can't reach the boring ranges, so the decode
/// stays a plain shift-or.
fn wide_boring_run_len(bytes: &[u8]) -> usize {
    let mut n = 0;
    while n + 3 <= bytes.len() {
        let b0 = bytes[n];
        if !(0xE0..=0xEF).contains(&b0)
            || bytes[n + 1] & 0xC0 != 0x80
            || bytes[n + 2] & 0xC0 != 0x80
        {
            break;
        }
        match char::from_u32(decode3_cp(&bytes[n..n + 3])) {
            Some(c) if fast_width(c) == Some(2) => n += 3,
            _ => break,
        }
    }
    n
}

/// Length (in bytes, multiple of 4) of the longest prefix of `bytes`
/// consisting of structurally-valid 4-byte UTF-8 sequences (lead
/// 0xF0-0xF4 + three continuations).  The feed loop's decode lane —
/// validity of the SCALAR (surrogate-free range) is settled by
/// `char::from_u32` at decode, mirroring the per-byte path exactly.
fn quad_run_len(bytes: &[u8]) -> usize {
    let mut n = 0;
    while n + 4 <= bytes.len() {
        if !(0xF0..=0xF4).contains(&bytes[n])
            || bytes[n + 1] & 0xC0 != 0x80
            || bytes[n + 2] & 0xC0 != 0x80
            || bytes[n + 3] & 0xC0 != 0x80
        {
            break;
        }
        n += 4;
    }
    n
}

/// Raw codepoint of a 3-byte UTF-8 sequence (caller checked shape).
#[inline(always)]
fn decode3_cp(b: &[u8]) -> u32 {
    (((b[0] & 0x0F) as u32) << 12) | (((b[1] & 0x3F) as u32) << 6) | (b[2] & 0x3F) as u32
}

#[inline(always)]
fn boring_width(ch: char) -> Option<u8> {
    match ch as u32 {
        0x20..=0x7E => Some(1),     // ASCII printable
        0x4E00..=0x9FFF => Some(2), // CJK Unified Ideographs
        0x3041..=0x3096 => Some(2), // hiragana (sans 3099/309A marks)
        0x30A1..=0x30FA => Some(2), // katakana
        0x30FC..=0x30FE => Some(2), // ー ヽ ヾ (sans 30FF)
        0x3001..=0x3029 => Some(2), // CJK punctuation 、。「」等
        0xFF01..=0xFF60 => Some(2), // fullwidth forms
        0x3400..=0x4DBF => Some(2), // CJK Extension A
        _ => None,
    }
}

/// `boring_width` plus precomposed Hangul syllables.  Syllables are
/// GBP LV/LVT — they can conjoin with FOLLOWING jamo (GB7/GB8), so
/// they don't belong in `boring_width`'s "never interacts" story,
/// but every pair drawn from {Other, LV, LVT} still has an
/// unconditional boundary between them (GB6-8 only join when the
/// NEXT char is jamo; GB9b needs a Prepend prev; neither class is
/// in here).  Combined with the invariant that a batch/fast commit
/// always leaves the LAST char buffered (so a following jamo, VS,
/// or mark meets an open cluster via the slow path), that makes
/// this the widest class the fast paths may commit early.
#[inline(always)]
fn fast_width(ch: char) -> Option<u8> {
    if let Some(w) = boring_width(ch) {
        return Some(w);
    }
    if matches!(ch as u32, 0xAC00..=0xD7A3) {
        return Some(2); // Hangul syllables (LV / LVT)
    }
    fast_pict_width(ch)
}

/// A pictograph that can only ever be a cluster of its own — the
/// emoji half of the fast class.
///
/// `cat` of emoji-dense output was the one scenario the batch lanes
/// never reached: a 4-byte scalar surrounded by single spaces makes
/// both the ASCII run (needs ≥ 2) and the quad run (needs ≥ 8) come
/// up short, so every glyph took the segmenter — three table walks
/// (`cluster_props` on the way in, `char_width` per codepoint on the
/// way out) to decide something the codepoint alone settles.
/// Measured 84 ns per non-ASCII character against 12.9 ns for CJK,
/// which reaches the wide lane.
///
/// Admitting one here is the same bargain `fast_width`'s doc
/// describes: the pair (this, anything in the fast class) has an
/// unconditional UAX #29 boundary, and the last character of a fast
/// commit always stays buffered, so a VS16 / ZWJ / skin-tone
/// modifier arriving next still meets an open cluster on the slow
/// path.  What must be excluded is anything whose boundary depends
/// on a neighbour:
///
///   - Regional indicators (GB12/13 pair into flags: 🇯🇵 is ONE cluster)
///   - Any codepoint that is not GBP=Other (Extend / ZWJ /
///     SpacingMark / Prepend all join across the boundary)
///
/// Width comes from the same `has_emoji_presentation` table
/// `char_width` consults, so the fast and slow paths cannot disagree
/// about how many cells the glyph takes.  A text-presentation
/// pictograph (⚠ without VS16) is deliberately NOT admitted: its
/// width depends on a variation selector that may still be coming.
#[inline]
fn fast_pict_width(ch: char) -> Option<u8> {
    let cp = ch as u32;
    // Below the symbol blocks nothing has emoji presentation, and
    // this is the branch every ASCII / Latin / CJK character takes.
    if cp < 0x2190 {
        return None;
    }
    // The two exclusions are ranges rather than a `gbp()` call: that
    // lookup was 23 % of emoji parse time once the fast path started
    // taking it per glyph, and across the whole codepoint space the
    // only emoji-presentation characters that are NOT GBP=Other are
    // these two blocks.  `the_fast_pictograph_class_never_outruns_the_tables`
    // re-derives that from the tables themselves, so a Unicode update
    // that adds a third one fails the build instead of silently
    // splitting a cluster.
    if (0x1F1E6..=0x1F1FF).contains(&cp)     // regional indicator — GB12/13 pairs it
        || (0x1F3FB..=0x1F3FF).contains(&cp)
    // skin-tone modifier — GBP=Extend
    {
        return None;
    }
    if !crate::emoji_presentation::has_emoji_presentation(cp) {
        return None;
    }
    Some(2)
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
    /// End the cluster being built.
    ///
    /// Nothing is written here any more: a glyph is committed the
    /// moment its width is known, and this only says that whatever
    /// arrives next cannot join it.  The name is kept because every
    /// caller means exactly that — a cursor move, an erase, a control
    /// code, the end of a batch.
    fn flush_cluster_keep_cursor(&mut self) {
        self.cluster_buf.clear();
        *self.cluster_fast = None;
        *self.cluster_anchor = None;
    }

    /// Same as [`flush_cluster_keep_cursor`] but also resets the
    /// segmenter — the next codepoint will be treated as a fresh
    /// cluster start.  Use at every non-print event (control byte,
    /// escape sequence, end-of-feed) so a cursor move or CSI doesn't
    /// fuse two visually distinct clusters across the operation.
    fn flush_cluster_for_break(&mut self) {
        self.flush_cluster_keep_cursor();
        self.grapheme_cursor.reset();
        // Empty buffer + freshly-reset cursor ARE in sync — clears
        // any fast-path debt so the next slow-path print doesn't
        // replay a stale flag.
        *self.seg_synced = true;
    }

    /// Batch-commit a printable-ASCII run (the feed loop's batch
    /// lane).  Semantically identical to `print`-ing each byte:
    /// every byte is `boring_width` class, so a UAX #29 boundary
    /// precedes each one unconditionally (sole exception: a pending
    /// cluster ending in a GB9b Prepend — the prologue detects that
    /// and we take the scalar path).  Interior bytes commit without
    /// touching `cluster_buf`; the LAST byte stays buffered exactly
    /// like the scalar fast path leaves it (a following VS16 /
    /// combining mark must still see it as the open cluster).
    fn print_ascii_run(&mut self, run: &[u8]) {
        debug_assert!(run.iter().all(|&b| (0x20..=0x7E).contains(&b)));
        // This lane exists to skip the per-character path, which is
        // exactly where `<u>` is recognised.  A run holding no `<`
        // keeps the whole optimisation; one that does gives up only
        // from the `<` onward.  A tag left half-matched by the
        // previous chunk has the same claim on this run's head.
        if self.u_tags && (!self.u_buf.is_empty() || run.contains(&b'<')) {
            let split = if self.u_buf.is_empty() {
                run.iter().position(|b| *b == b'<').unwrap_or(0)
            } else {
                0
            };
            let (head, tail) = run.split_at(split);
            if !head.is_empty() {
                self.print_ascii_run(head);
            }
            for &b in tail {
                self.print(b as char);
            }
            return;
        }
        if !self.batch_prologue_flush(false) {
            for &b in run {
                self.print(b as char);
            }
            return;
        }
        let (last, body) = run.split_last().expect("run_len >= 2");
        self.write_ascii_body(body);
        // The batch used to leave its last byte buffered so a mark
        // arriving next could join it.  It is committed now, and the
        // anchor is what a mark joins.
        self.commit_cluster_head(*last as char, 1, true);
    }

    /// Flush the pending cluster ahead of a batch run IF a boundary
    /// before the run head is unconditional.  Returns false when the
    /// cluster could legally absorb the head — GB9b (buffer ends in a
    /// Prepend) for any head, GB6-8 (buffer ends in jamo / syllable)
    /// for a Hangul-syllable head — and the caller must fall back to
    /// the scalar path for the run.
    fn batch_prologue_flush(&mut self, head_is_hangul: bool) -> bool {
        if self.cluster_buf.is_empty() {
            return true;
        }
        let mut it = self.cluster_buf.chars();
        let first = it.next().expect("non-empty buffer");
        if it.next().is_none() && fast_width(first).is_some() {
            // fast-class × fast-class: unconditional boundary (GB6-8
            // join only when the NEXT char is jamo, GB9b needs a
            // Prepend prev — neither is in the fast class).
            self.flush_cluster_keep_cursor();
            return true;
        }
        use crate::unicode_data::{GBP, gbp};
        let last = self.cluster_buf.chars().next_back().expect("non-empty");
        let lp = gbp(last as u32);
        if lp == GBP::Prepend {
            return false; // GB9b: Prepend × any joins
        }
        if head_is_hangul && matches!(lp, GBP::L | GBP::V | GBP::T | GBP::LV | GBP::LVT) {
            return false; // GB6-8: jamo / syllable can conjoin a syllable
        }
        self.flush_cluster_keep_cursor();
        true
    }

    /// Row-sliced bulk write of single-width ASCII glyphs.  Chains
    /// the same DECAWM semantics as repeated `write_glyph(_, 1)`:
    /// wrap immediately when more bytes follow, defer the wrap when
    /// the run ends exactly at the right edge.
    fn write_ascii_body(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.take_pending_wrap();
        let cols = self.grid.cols();
        let rows = self.grid.rows();
        let mut idx = 0;
        while idx < bytes.len() {
            let (col, row) = self.grid.cursor();
            let n = ((cols - col) as usize).min(bytes.len() - idx);
            self.grid
                .set_row_run_ascii(col, row, *self.attrs, &bytes[idx..idx + n]);
            idx += n;
            let next_col = col + n as u16;
            if next_col < cols {
                self.grid.set_cursor(next_col, row);
            } else if idx < bytes.len() {
                // Row filled and more bytes follow — wrap now (the
                // per-char path would consume the deferred wrap on
                // the next byte anyway; batching collapses that).
                self.wrap_to_next_row(row, rows);
            } else {
                // Run ends exactly at the right edge — defer the wrap,
                // mirroring `write_glyph`.
                self.grid.set_cursor(cols - 1, row);
                *self.pending_wrap = true;
            }
        }
    }

    /// Batch-commit a wide fast-class run (multiple 3-byte UTF-8
    /// sequences, each a `fast_width == 2` char — the feed loop's
    /// wide lane).  Same shape as `print_ascii_run`: prologue-flush
    /// the pending cluster (falling back to the scalar path when it
    /// could legally absorb the head), bulk-write the body, leave
    /// the LAST char buffered so a following VS15/VS16/jamo still
    /// sees an open cluster.
    fn print_wide_run(&mut self, run: &[u8]) {
        debug_assert!(run.len() % 3 == 0 && run.len() >= 6);
        // A `<` held from the previous chunk cannot be a tag once a
        // wide character follows; print it before the bulk write, or
        // it would sit in the buffer and attach itself to whatever
        // ASCII comes next.
        if !self.u_buf.is_empty() {
            let held = std::mem::take(self.u_buf);
            for c in held.chars() {
                self.print_glyph(c);
            }
        }
        let head =
            char::from_u32(decode3_cp(&run[..3])).expect("scan admitted only fast-class scalars");
        let head_is_hangul = matches!(head as u32, 0xAC00..=0xD7A3);
        if !self.batch_prologue_flush(head_is_hangul) {
            for chunk in run.chunks_exact(3) {
                let c = char::from_u32(decode3_cp(chunk))
                    .expect("scan admitted only fast-class scalars");
                self.print(c);
            }
            return;
        }
        let body = &run[..run.len() - 3];
        self.write_wide_body(body);
        let last = char::from_u32(decode3_cp(&run[run.len() - 3..]))
            .expect("scan admitted only fast-class scalars");
        self.commit_cluster_head(last, 2, true);
    }

    /// Row-sliced bulk write of width-2 glyphs (lead cell + NUL trail
    /// pad), chaining the same DECAWM semantics as repeated
    /// `write_glyph(_, 2)`: NUL-pad an unusable last column before
    /// wrapping, wrap immediately when more chars follow, defer the
    /// wrap when the run ends exactly at the right edge.
    fn write_wide_body(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.take_pending_wrap();
        let cols = self.grid.cols();
        let rows = self.grid.rows();
        let mut idx = 0;
        while idx < bytes.len() {
            let (col, row) = self.grid.cursor();
            let space = cols - col;
            if space < 2 {
                // A wide glyph can't fit in the last column — pad it
                // (when empty) and wrap, mirroring `write_glyph`.
                if self.grid.cell(cols - 1, row) == Cell::default() {
                    self.grid.set_cell(
                        cols - 1,
                        row,
                        Cell {
                            ch: '\0',
                            attrs: *self.attrs,
                        },
                    );
                }
                self.wrap_to_next_row(row, rows);
                continue;
            }
            let fit = ((space / 2) as usize).min((bytes.len() - idx) / 3);
            let attrs = *self.attrs;
            let cells = self.grid.row_cells_mut(col, row, fit * 2);
            for k in 0..fit {
                let c = char::from_u32(decode3_cp(&bytes[idx + k * 3..idx + k * 3 + 3]))
                    .expect("scan admitted only boring scalars");
                cells[k * 2] = Cell { ch: c, attrs };
                cells[k * 2 + 1] = Cell { ch: '\0', attrs };
            }
            idx += fit * 3;
            let next_col = col + (fit * 2) as u16;
            if next_col < cols {
                self.grid.set_cursor(next_col, row);
            } else if idx < bytes.len() {
                self.wrap_to_next_row(row, rows);
            } else {
                self.grid.set_cursor(cols - 1, row);
                *self.pending_wrap = true;
            }
        }
    }

    /// Scroll-or-step to column 0 of the next row and mark it as a
    /// soft wrap continuation.  Shared tail of the batch lanes' and
    /// `write_glyph`'s wrap paths.
    fn wrap_to_next_row(&mut self, row: u16, rows: u16) {
        let bot = *self.scroll_bot;
        if row == bot {
            self.region_scroll_up(1);
            self.grid.set_cursor(0, row);
        } else if row + 1 < rows {
            self.grid.set_cursor(0, row + 1);
        } else {
            self.grid.set_cursor(0, rows - 1);
        }
        let (_, r) = self.grid.cursor();
        self.grid.set_row_wrapped(r, true);
    }

    /// Re-seed the segmenter after the ASCII fast path skipped it.
    /// The fast path maintains the invariant that an unsynced buffer
    /// holds at most one (ASCII) codepoint, so the replay is O(1).
    fn resync_segmenter(&mut self) {
        if !*self.seg_synced {
            self.grapheme_cursor.reset();
            for c in self.cluster_buf.chars() {
                self.grapheme_cursor.step(c);
            }
            *self.seg_synced = true;
        }
    }

    /// Commit one already-segmented glyph (codepoint + cell width) to
    /// the grid at the cursor, handling the DECAWM deferred-wrap and
    /// wide-char wrap edge cases.  Body extracted from the old
    /// per-codepoint `print` so the cluster flush path and any future
    /// non-parser writer can share the same cursor-advance logic.
    /// Commit one glyph and return the cell it landed in, so the
    /// caller can anchor to it — a codepoint that arrives later and
    /// belongs to the same cluster amends that cell rather than
    /// starting a new one.
    fn write_glyph(&mut self, ch: char, w: u8) -> (u16, u16) {
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
                    Cell {
                        ch: '\0',
                        attrs: *self.attrs,
                    },
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
        self.grid.set_cell(
            col,
            row,
            Cell {
                ch,
                attrs: *self.attrs,
            },
        );
        if w == 2 {
            self.grid.set_cell(
                col + 1,
                row,
                Cell {
                    ch: '\0',
                    attrs: *self.attrs,
                },
            );
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
        (col, row)
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
            self.grid
                .scroll_up_region(top, bot, lines, blank_with(*self.attrs));
        }
    }

    fn region_scroll_down(&mut self, lines: u16) {
        let top = *self.scroll_top;
        let bot = *self.scroll_bot;
        self.grid
            .scroll_down_region(top, bot, lines, blank_with(*self.attrs));
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
        // VT spec says alt buffer has no scrollback,but user-perception
        // wise iTerm2 / Kitty / Alacritty all let scroll wheel browse
        // alt-screen history while inside a TUI(claudecode 长对话刷出
        // 屏顶 ≠ 没法再看).Use the same `DEFAULT_SCROLLBACK_LINES`
        // ring 主屏用 — claudecode 等 TUI 输出量大,小 ring 装不下
        // 一次会话.Drop wholesale on `?1049l`(exit_alt_screen 把整
        // 个 grid 替换回 saved_main).
        let alt = Grid::with_scrollback(cols, rows, DEFAULT_SCROLLBACK_LINES);
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
            // DECSET ?1004 — focus reporting.  Turning it OFF also
            // forgets what was last reported, so a program that turns
            // it back on is told the current state rather than being
            // held to a stale one.
            1004 => {
                *self.focus_reporting = set;
                if !set {
                    *self.focus_reported = None;
                }
            }
            // Accept silently — these modes have no rendering side
            // effect we model, but apps want them to "succeed" rather
            // than no-op silently. Listed explicitly so future audits
            // see them.
            //   1000 / 1002 / 1003 / 1006 / 1015 — mouse reporting modes
            //   1004                — focus reporting in/out events
            //   2031                — color scheme update notifications
            // 1000 / 1002 / 1003 — mouse reporting modes:claudecode 等
            // TUI 进 alt-screen 后 set 这些 + 1006(SGR encoding),期望
            // marspot 收到 wheel/click 后 encode 成 escape sequence 写
            // 回 PTY.之前(commit 176e4f4 extract 起)全部 stub 成
            // no-op,wheel 在 claudecode 内永远滚不动是这个根因.
            1000 => {
                *self.mouse_tracking_mode = if set {
                    MouseTrackingMode::X11
                } else {
                    MouseTrackingMode::Off
                }
            }
            1002 => {
                *self.mouse_tracking_mode = if set {
                    MouseTrackingMode::ButtonEvent
                } else {
                    MouseTrackingMode::Off
                }
            }
            1003 => {
                *self.mouse_tracking_mode = if set {
                    MouseTrackingMode::AnyEvent
                } else {
                    MouseTrackingMode::Off
                }
            }
            1006 => *self.mouse_sgr_encoding = set,
            // DEC 1007 — alternate scroll: the wheel is the arrow
            // keys on this screen.  See `Terminal::alt_scroll`.
            1007 => *self.alt_scroll = set,
            // DEC 2026 — synchronized output.  `h` opens a batch, `l`
            // closes it; the presenter holds frames in between so a
            // half-drawn screen is never shown.  See
            // `Terminal::sync_output`.
            2026 => {
                *self.sync_output = set;
                if set {
                    *self.uses_sync_output = true;
                }
            }
            // 1015 / 1004 / 2031 — silently accept but no-op.
            // 2031 (colour-scheme change notification) is accepted and
            // not reported on: marspot has one palette and it never
            // changes, so there is no event to send.  A reporting path
            // with nothing to report would be invented work — and the
            // real session never queried the scheme either (one `CSI 6
            // n` in 22 MB, which is a cursor-position request).
            1015 | 2031 => {}
            _ => {} // unhandled DEC private mode — silently skip
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
        self.print_glyph(ch);
    }

    fn execute(&mut self, byte: u8) {
        // A control code ends the text run, so a `<` still waiting to
        // become a tag never will.  Without this, a line ending in one
        // swallows it until the next printable character.
        self.flush_u_buf();
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
        self.flush_u_buf();
        self.flush_cluster_for_break();
        trace_seq("ESC", intermediates, &[], byte);
        *self.pending_wrap = false;
        match byte {
            // DECSC — save cursor (position + SGR attrs).
            b'7' => {
                let (col, row) = self.grid.cursor();
                *self.saved_cursor = Some(SavedCursor {
                    col,
                    row,
                    attrs: *self.attrs,
                });
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
        self.flush_u_buf();
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
                // CUU: cursor up by N (default 1).
                let n = param(params, 0, 1);
                self.grid.set_cursor(col, row.saturating_sub(n));
            }
            b'B' => {
                // CUD: cursor down by N.  set_cursor clamps at rows-1.
                let n = param(params, 0, 1);
                self.grid
                    .set_cursor(col, row.saturating_add(n).min(rows - 1));
            }
            b'C' => {
                // CUF: cursor forward (right).
                let n = param(params, 0, 1);
                self.grid
                    .set_cursor(col.saturating_add(n).min(cols - 1), row);
            }
            b'E' => {
                // CNL: cursor next line — down N, column 0.
                let n = param(params, 0, 1);
                self.grid.set_cursor(0, row.saturating_add(n).min(rows - 1));
            }
            b'F' => {
                // CPL: cursor previous line — up N, column 0.  Homebrew's
                // concurrent-download display redraws itself with
                // `\033[{n}F` (Tty.move_cursor_up_beginning); with this
                // missing the cursor never moved up and every refresh
                // APPENDED its lines — the 2026-07-28 "brew progress
                // scrolls forever" field report.
                let n = param(params, 0, 1);
                self.grid.set_cursor(0, row.saturating_sub(n));
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
                apply_sgr(self.attrs, params);
            }
            b's' => {
                // SCO save cursor.  Same semantics as DECSC (ESC 7).
                let (col, row) = self.grid.cursor();
                *self.saved_cursor = Some(SavedCursor {
                    col,
                    row,
                    attrs: *self.attrs,
                });
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
                    self.grid
                        .scroll_down_region(row, *self.scroll_bot, n, blank_with(*self.attrs));
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
                    self.grid
                        .scroll_up_region(row, *self.scroll_bot, n, blank_with(*self.attrs));
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
                        // (row, col), 1-based.  No DECOM here because
                        // marspot has no origin mode to be relative to
                        // — absolute is the only reading available, and
                        // it is what every app assumes when DECOM is
                        // off (the default everywhere).
                        let (c0, r0) = self.grid.cursor();
                        let (row, col) = (r0 + 1, c0 + 1);
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
        self.flush_u_buf();
        self.flush_cluster_for_break();
        // F3+3.6 — OSC 7 push-based cwd reporting removed; the
        // LayoutModal now pull-fetches cwd via `proc_pidinfo` only
        // when the user opens it (one syscall per pane, not on the
        // hot path).
        let (num, rest) = match data.iter().position(|&b| b == b';') {
            Some(i) => (&data[..i], &data[i + 1..]),
            None => (data, &data[data.len()..]),
        };
        let num = std::str::from_utf8(num).ok().and_then(|s| s.parse::<u16>().ok());
        match num {
            // 0 = icon name + window title, 2 = window title.  Stored,
            // not displayed: a pane's label in marspot is DERIVED (its
            // directory), deliberately, so that there is no second
            // source of truth about which pane is which.  Wiring this
            // into the title strip is a product decision, not a
            // protocol one — but a program that says what it is
            // shouldn't have that thrown away in the parser.
            Some(0) | Some(2) => {
                if let Ok(t) = std::str::from_utf8(rest) {
                    if self.osc_title != t {
                        self.osc_title.clear();
                        // Bounded: a title is a label, and a program
                        // that sends a megabyte of one is not going to
                        // get a megabyte of storage for it.
                        self.osc_title.extend(t.chars().take(256));
                    }
                }
            }
            // 10 = default foreground, 11 = default background, and
            // `?` as the value makes it a QUERY.  An unanswered query
            // is how a TUI ends up guessing whether it is on a dark or
            // light terminal — the same shape as the DA1 stall that
            // produced blank rows and misaligned chrome.  Answer in
            // xterm's own form, and echo the OSC number back so a
            // batched query is unambiguous.
            Some(n @ (10 | 11)) if rest.starts_with(b"?") => {
                let c = if n == 10 { crate::palette::FG } else { crate::palette::BG };
                let reply = format!("\x1b]{n};{}\x07", crate::palette::xterm_rgb(c));
                self.pending_response.extend_from_slice(reply.as_bytes());
                self.record_response("OSC-COLOR");
            }
            _ => {
                lx_debug!(
                    "term.osc.unsupported",
                    "OSC payload with no handler",
                    bytes = data.len(),
                    num = num.unwrap_or(u16::MAX) as u64
                );
            }
        }
    }
}

/// How much text a `<u>` may hold before we give up on its close.
///
/// A sentence is far short of this; a span that runs longer has almost
/// certainly lost its closing tag, and holding output hostage waiting
/// for one is worse than printing the tag.
const U_SPAN_CAP: usize = 1024;

/// What a buffer starting with `<` has turned out to be.
enum UTag {
    /// `<u>` — start collecting what it wraps.
    Open,
    /// Still a prefix of one of them.
    Maybe,
    /// It is not, and never will be.
    No,
}

fn u_tag_verdict(buf: &str) -> UTag {
    match buf {
        "<u>" => UTag::Open,
        "<" | "<u" => UTag::Maybe,
        // A closing tag with nothing open is just text.
        _ => UTag::No,
    }
}

impl<'a> Handler<'a> {
    /// One character, while a `<u>` is being recognised or collected.
    fn u_step(&mut self, ch: char) {
        if *self.u_open {
            // Collecting the span.  `</u>` closes it; anything else
            // just accumulates until it does or the cap says stop.
            self.u_buf.push(ch);
            if self.u_buf.ends_with("</u>") {
                let span: String = self.u_buf[..self.u_buf.len() - "</u>".len()].to_string();
                self.u_buf.clear();
                *self.u_open = false;
                // The text before the tag keeps the attributes it
                // arrived with; flush it before changing them.
                self.flush_cluster_for_break();
                let was = self.attrs.underline;
                self.attrs.underline = true;
                for c in span.chars() {
                    self.print_glyph(c);
                }
                self.flush_cluster_for_break();
                self.attrs.underline = was;
                return;
            }
            if self.u_buf.len() > U_SPAN_CAP {
                self.u_bail();
            }
            return;
        }
        self.u_buf.push(ch);
        match u_tag_verdict(self.u_buf) {
            UTag::Open => {
                self.u_buf.clear();
                *self.u_open = true;
            }
            UTag::Maybe => {}
            UTag::No => {
                let held = std::mem::take(self.u_buf);
                for c in held.chars() {
                    self.print_glyph(c);
                }
            }
        }
    }

    /// Give up on a span: print the opening tag and everything after
    /// it exactly as it arrived.
    fn u_bail(&mut self) {
        *self.u_open = false;
        let held = std::mem::take(self.u_buf);
        for c in "<u>".chars() {
            self.print_glyph(c);
        }
        for c in held.chars() {
            self.print_glyph(c);
        }
    }

    /// Print anything still held as a possible `<u>`, unchanged.
    ///
    /// Called wherever the run of printable text ends — a control
    /// byte, an escape sequence — because a tag cannot span one.
    fn flush_u_buf(&mut self) {
        if *self.u_open {
            self.u_bail();
            return;
        }
        if self.u_buf.is_empty() {
            return;
        }
        let held = std::mem::take(self.u_buf);
        for c in held.chars() {
            self.print_glyph(c);
        }
    }

    fn print_glyph(&mut self, ch: char) {
        // FAST CLASS — the boundary BEFORE one of these is
        // unconditional (UAX #29 has no rule joining Other+Other, and
        // the class is chosen so LV/LVT and emoji-presentation
        // pictographs behave the same way), so no segmenter is needed
        // to know this starts a new cluster.  Commit it now.
        // …but only while the cluster currently open is itself of
        // that class, or none is.  After a ZWJ or any other extender
        // the boundary is NOT unconditional — GB11 joins ZWJ to the
        // pictograph after it — so `👨 ZWJ 👩` has to reach the
        // segmenter even though `👩` is fast class on its own.
        if let Some(w) = fast_width(ch) {
            if self.cluster_buf.is_empty() || self.cluster_fast.is_some() {
                self.commit_cluster_head(ch, w, true);
                return;
            }
        }
        // SLOW PATH — this codepoint may EXTEND what was just drawn
        // (`e` + ́ , ⚠ + VS16, 👨 + ZWJ + 👩, LV + jamo, क + virama),
        // and only the segmenter knows.
        self.resync_segmenter();
        if self.grapheme_cursor.step(ch) {
            // A boundary: its own cluster, committed at its own width.
            // Anything that widens it arrives later and amends it.
            let w = crate::grapheme::cluster_width(ch.encode_utf8(&mut [0u8; 4]));
            if w > 0 {
                self.commit_cluster_head(ch, w as u8, false);
            } else {
                // A zero-width codepoint with a boundary before it has
                // nothing to attach to — it is not drawn, but it does
                // open a cluster the next codepoint may extend.
                self.cluster_buf.clear();
                self.cluster_buf.push(ch);
                *self.cluster_fast = None;
                *self.cluster_anchor = None;
            }
            return;
        }
        // No boundary: `ch` belongs to the cluster already on screen.
        self.cluster_buf.push(ch);
        *self.cluster_fast = None;
        let w = crate::grapheme::cluster_width(self.cluster_buf);
        self.widen_anchor(w as u8);
    }

    /// Draw `ch` as the first codepoint of a new cluster and remember
    /// where it went.
    /// `fast` says the head is of the class whose successors need no
    /// segmenter.  A head that reached here through the segmenter is
    /// NOT that, even when it would have qualified alone: what follows
    /// it may still join.
    fn commit_cluster_head(&mut self, ch: char, w: u8, fast: bool) {
        let at = self.write_glyph(ch, w);
        self.cluster_buf.clear();
        self.cluster_buf.push(ch);
        *self.cluster_fast = if fast { Some((ch, w)) } else { None };
        *self.cluster_anchor = Some((at.0, at.1, w));
        *self.seg_synced = false;
    }

    /// The cluster turned out to be wider than its first codepoint —
    /// `⚠` is one cell, `⚠️` is two.  Claim the cell after it and push
    /// the cursor along, but only when the glyph is still where it was
    /// left: anything that moved the cursor clears the anchor, and a
    /// glyph already at the right edge has nowhere to grow into (it
    /// keeps the width it was drawn at, which is what it looked like
    /// before this codepoint arrived).
    fn widen_anchor(&mut self, w: u8) {
        let Some((col, row, cur)) = *self.cluster_anchor else {
            return;
        };
        if w <= cur || w != 2 {
            return;
        }
        let cols = self.grid.cols();
        if col + 1 >= cols {
            return;
        }
        self.grid.set_cell(
            col + 1,
            row,
            Cell {
                ch: '\0',
                attrs: *self.attrs,
            },
        );
        if self.grid.cursor() == (col + 1, row) {
            self.grid.set_cursor(col + 2, row);
        }
        *self.cluster_anchor = Some((col, row, w));
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
        std::env::var("MARSPOT_TRACE_ESC").ok().and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
                .ok()
                .map(Mutex::new)
        })
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
    // BCE inherits ONLY the background colour — that is the whole of
    // the xterm contract.  Stamping the full SGR state into erase /
    // scroll-fill blanks let UNDERLINE leak: printing a `\e[4m` URL
    // that wrapped on the bottom row scrolled a fresh row into
    // existence while underline was still on, so every blank in it
    // was born underlined and the tail of the line dragged a rule to
    // the window edge (2026-07-29 field report — the omz update
    // banner grew stray horizontal lines through its links and logo).
    // bold/italic/underline/reverse/dim are glyph properties; a blank
    // has no glyph, and reverse on a blank would even paint a solid
    // fg-coloured block no real terminal shows on clear.
    Cell {
        ch: ' ',
        attrs: CellAttrs {
            bg: attrs.bg,
            ..CellAttrs::default()
        },
    }
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
            22 => {
                attrs.bold = false;
                attrs.dim = false;
            }
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
            Some((
                Color::Rgb(r.min(255) as u8, g.min(255) as u8, b.min(255) as u8),
                4,
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    /// The fast pictograph class is allowed to commit a character as
    /// a cluster of its own without asking the segmenter.  That is
    /// only sound while every character it admits is (a) GBP=Other,
    /// so no neighbour can join across the boundary, and (b) two
    /// cells wide by the same table `char_width` reads — otherwise
    /// the fast and slow paths would disagree about layout.
    ///
    /// Walks the entire codepoint space rather than a sample: the
    /// class is defined by range exclusions, and the thing that
    /// would break it is a Unicode update adding a character the
    /// ranges do not cover.  A sample of today's emoji cannot see
    /// that; this can.
    #[test]
    fn the_fast_pictograph_class_never_outruns_the_tables() {
        use crate::unicode_data::{GBP, gbp};
        for cp in 0..0x11_0000u32 {
            let Some(ch) = char::from_u32(cp) else {
                continue;
            };
            let Some(w) = super::fast_pict_width(ch) else {
                continue;
            };
            assert_eq!(
                gbp(cp),
                GBP::Other,
                "U+{cp:04X} admitted to the fast class but joins its neighbours"
            );
            assert!(
                crate::emoji_presentation::has_emoji_presentation(cp),
                "U+{cp:04X} admitted without emoji presentation"
            );
            assert_eq!(w, 2, "U+{cp:04X} fast width disagrees with itself");
            assert_eq!(
                crate::grid::char_width(ch),
                2,
                "U+{cp:04X} fast width disagrees with char_width"
            );
        }
    }

    use super::*;

    fn term_with(cols: u16, rows: u16, bytes: &[u8]) -> Terminal {
        let mut t = Terminal::new(cols, rows);
        t.feed(bytes);
        t
    }

    /// Pins the fast path's hardcoded class facts against the real
    /// UCD tables: EVERY codepoint `boring_width` claims must be
    /// GBP::Other, not Extended_Pictographic, InCB::None, and have
    /// exactly the claimed single-char cluster_width.  Walking the
    /// whole BMP+ExtA space keeps the range list itself honest — a
    /// future `bin/regen-unicode-tables.sh` regen that invalidated a
    /// member fails here before the fast path can mis-render.
    #[test]
    fn fast_path_class_is_sound() {
        use crate::unicode_data::{GBP, InCB, gbp, incb, is_extended_pictographic};
        for cp in 0x20u32..=0xFFFF {
            let Some(ch) = char::from_u32(cp) else {
                continue;
            };
            let Some(w) = boring_width(ch) else { continue };
            assert_eq!(gbp(cp), GBP::Other, "U+{cp:04X} gbp");
            assert!(!is_extended_pictographic(cp), "U+{cp:04X} pictographic");
            assert_eq!(incb(cp), InCB::None, "U+{cp:04X} incb");
            assert_eq!(
                crate::grapheme::cluster_width(&ch.to_string()),
                w,
                "U+{cp:04X} width"
            );
        }
        // Hangul syllables ride the wider `fast_width` class: LV/LVT
        // only, never pictographic / InCB, always 2 cells.
        for cp in 0xAC00u32..=0xD7A3 {
            let ch = char::from_u32(cp).unwrap();
            assert_eq!(fast_width(ch), Some(2), "U+{cp:04X} fast_width");
            assert!(
                matches!(gbp(cp), GBP::LV | GBP::LVT),
                "U+{cp:04X} gbp must be LV/LVT"
            );
            assert!(!is_extended_pictographic(cp), "U+{cp:04X} pictographic");
            assert_eq!(incb(cp), InCB::None, "U+{cp:04X} incb");
            assert_eq!(
                crate::grapheme::cluster_width(&ch.to_string()),
                2,
                "U+{cp:04X} width"
            );
        }
        // Spot-check deliberate exclusions.
        assert!(boring_width('\u{1F}').is_none()); // C0
        assert!(boring_width('\u{7F}').is_none()); // DEL
        assert!(boring_width('é').is_none()); // Latin-1 (can NFD-combine)
        assert!(boring_width('\u{3099}').is_none()); // combining kana mark
        assert!(boring_width('\u{AC00}').is_none()); // Hangul stays out of boring
        assert!(boring_width('⭐').is_none()); // pictographic
        assert!(fast_width('\u{1100}').is_none()); // L jamo — GB6 joins forward
        assert!(fast_width('\u{11A8}').is_none()); // T jamo — GB8 joins backward
    }

    /// Hangul through the fast/batch lanes: precomposed syllables
    /// batch as width-2 cells; conjoining jamo still fuse via the
    /// slow path (the last run char stays buffered).
    #[test]
    fn hangul_fast_lane_semantics() {
        // 안녕하세요 — five LVT/LV syllables, 2 cells each.
        let t = term_with(20, 4, "안녕하세요".as_bytes());
        for (i, ch) in "안녕하세요".chars().enumerate() {
            assert_eq!(t.grid().cell(i as u16 * 2, 0).ch, ch);
            assert_eq!(t.grid().cell(i as u16 * 2 + 1, 0).ch, '\0');
        }
        assert_eq!(t.grid().cursor(), (10, 0));

        // LV syllable + trailing T jamo conjoin into ONE cluster
        // (GB8 via the buffered last char), not two cell pairs.
        let t = term_with(20, 4, "\u{AC00}\u{11A8}z".as_bytes());
        assert_eq!(t.grid().cell(0, 0).ch, '\u{AC00}');
        assert_eq!(t.grid().cell(2, 0).ch, 'z');

        // L + V jamo compose via the slow path (neither is fast class).
        let t = term_with(20, 4, "\u{1100}\u{1161}z".as_bytes());
        assert_eq!(t.grid().cell(0, 0).ch, '\u{1100}');
        assert_eq!(t.grid().cell(2, 0).ch, 'z');
    }

    /// GB9b regression: a pending cluster ending in a Prepend char
    /// absorbs the following printable — the batch lanes must detect
    /// that and fall back instead of flushing early.
    #[test]
    fn prepend_cluster_absorbs_batch_head() {
        // U+0600 ARABIC NUMBER SIGN (GBP Prepend) + "1234":
        // old scalar semantics = cluster [0600, '1'] commits base
        // U+0600 at cluster_width, then '2','3','4' follow.
        let w = crate::grapheme::cluster_width("\u{0600}1") as u16;
        let t = term_with(20, 4, "\u{0600}1234".as_bytes());
        assert_eq!(t.grid().cell(0, 0).ch, '\u{0600}');
        assert_eq!(t.grid().cell(w, 0).ch, '2');
        assert_eq!(t.grid().cell(w + 1, 0).ch, '3');
        assert_eq!(t.grid().cell(w + 2, 0).ch, '4');
    }

    /// Batch-lane specifics: right-edge wrap semantics must chain
    /// exactly like repeated single-width `write_glyph` calls.
    #[test]
    fn batch_ascii_run_wrap_semantics() {
        // 10-col grid, 25-char run → rows fill + wrapped flags.
        let t = term_with(10, 4, b"abcdefghijklmnopqrstuvwxy");
        assert_eq!(t.grid().cell(0, 0).ch, 'a');
        assert_eq!(t.grid().cell(9, 0).ch, 'j');
        assert_eq!(t.grid().cell(0, 1).ch, 'k');
        assert_eq!(t.grid().cell(9, 1).ch, 't');
        assert_eq!(t.grid().cell(0, 2).ch, 'u');
        assert_eq!(t.grid().cell(4, 2).ch, 'y');
        assert!(t.grid().row_wrapped(1));
        assert!(t.grid().row_wrapped(2));
        assert_eq!(t.grid().cursor(), (5, 2));

        // Run ending EXACTLY at the right edge defers the wrap: cursor
        // parks on the last column, next glyph wraps, a CR instead
        // cancels without advancing (classic DECAWM).
        let t = term_with(10, 4, b"0123456789");
        assert_eq!(t.grid().cursor(), (9, 0));
        let t = term_with(10, 4, b"0123456789X");
        assert_eq!(t.grid().cell(0, 1).ch, 'X');
        let t = term_with(10, 4, b"0123456789\r\nY");
        assert_eq!(t.grid().cell(0, 1).ch, 'Y');
        assert_eq!(t.grid().cell(9, 0).ch, '9');

        // Wide cluster immediately before a batch run: run starts
        // after the trail pad.
        let t = term_with(20, 4, "⭐abcdef".as_bytes());
        assert_eq!(t.grid().cell(0, 0).ch, '⭐');
        assert_eq!(t.grid().cell(2, 0).ch, 'a');
        assert_eq!(t.grid().cell(7, 0).ch, 'f');

        // SGR mid-stream splits the run at the escape; attrs apply to
        // the following batch.
        let t = term_with(20, 4, b"ab\x1b[1mcd");
        assert_eq!(t.grid().cell(2, 0).ch, 'c');
        assert!(t.grid().cell(2, 0).attrs.bold);
        assert!(!t.grid().cell(1, 0).attrs.bold);
    }

    /// Wide batch lane: CJK runs commit through `print_wide_run` with
    /// the same wrap semantics as repeated `write_glyph(_, 2)`.
    #[test]
    fn batch_wide_run_wrap_semantics() {
        // 10-col grid: 4 CJK chars/row, 9 chars → 3 rows (8 cells +
        // 2-cell tail per row boundary).
        let t = term_with(10, 4, "一二三四五六七八九".as_bytes());
        assert_eq!(t.grid().cell(0, 0).ch, '一');
        assert_eq!(t.grid().cell(8, 0).ch, '五');
        assert_eq!(t.grid().cell(9, 0).ch, '\0'); // trail pad
        assert_eq!(t.grid().cell(0, 1).ch, '六');
        assert!(t.grid().row_wrapped(1));
        assert_eq!(t.grid().cell(6, 1).ch, '九');
        assert_eq!(t.grid().cursor(), (8, 1));

        // Odd column start: ASCII then CJK — last column unusable,
        // NUL pad + wrap, identical to the scalar wide path.
        let t = term_with(5, 4, "ab一二".as_bytes());
        assert_eq!(t.grid().cell(0, 0).ch, 'a');
        assert_eq!(t.grid().cell(2, 0).ch, '一');
        assert_eq!(t.grid().cell(4, 0).ch, '\0'); // unusable last col pad
        assert_eq!(t.grid().cell(0, 1).ch, '二');
        assert!(t.grid().row_wrapped(1));

        // Run ends exactly at the right edge → deferred wrap.
        let t = term_with(4, 4, "一二".as_bytes());
        assert_eq!(t.grid().cursor(), (3, 0));
        let t = term_with(4, 4, "一二三".as_bytes());
        assert_eq!(t.grid().cell(0, 1).ch, '三');

        // CJK then combining mark: last run char stays the open
        // cluster, mark attaches (and is dropped per Phase 1) without
        // a stray cell.
        let t = term_with(20, 4, "水木\u{3099}z".as_bytes());
        assert_eq!(t.grid().cell(0, 0).ch, '水');
        assert_eq!(t.grid().cell(2, 0).ch, '木');
        assert_eq!(t.grid().cell(4, 0).ch, 'z');
    }

    /// The fast path must be behaviourally invisible: interleaving
    /// ASCII with cluster-forming codepoints in every adjacency order
    /// yields the same grid as the pure-slow-path semantics.
    #[test]
    fn ascii_fast_path_matches_slow_path_semantics() {
        // ASCII then combining mark (fast → slow transition): the
        // mark arrives with an unsynced segmenter and must not fuse
        // wrongly or emit a stray cell.
        let t = term_with(20, 4, "ae\u{0301}b".as_bytes());
        assert_eq!(t.grid().cell(0, 0).ch, 'a');
        assert_eq!(t.grid().cell(1, 0).ch, 'e'); // base committed, mark dropped (Phase 1)
        assert_eq!(t.grid().cell(2, 0).ch, 'b');
        assert_eq!(t.grid().cursor(), (3, 0));

        // Wide cluster then ASCII (slow → fast transition).
        let t = term_with(20, 4, "⭐x".as_bytes());
        assert_eq!(t.grid().cell(0, 0).ch, '⭐');
        assert_eq!(t.grid().cell(2, 0).ch, 'x');

        // ASCII split across feeds — end-of-feed flush + fresh feed.
        let mut t = Terminal::new(20, 4);
        t.feed(b"ab");
        t.feed(b"cd");
        for (i, ch) in ['a', 'b', 'c', 'd'].into_iter().enumerate() {
            assert_eq!(t.grid().cell(i as u16, 0).ch, ch);
        }

        // ASCII at end of feed, combining mark opens the next feed:
        // same visible result as the single-feed case above.
        let mut t = Terminal::new(20, 4);
        t.feed(b"e");
        t.feed("\u{0301}z".as_bytes());
        assert_eq!(t.grid().cell(0, 0).ch, 'e');
        assert_eq!(t.grid().cell(1, 0).ch, 'z');

        // VS16 emoji sandwiched in ASCII keeps its 2-cell width.
        let t = term_with(20, 4, "a\u{26A0}\u{FE0F}b".as_bytes());
        assert_eq!(t.grid().cell(0, 0).ch, 'a');
        assert_eq!(t.grid().cell(1, 0).ch, '\u{26A0}');
        assert_eq!(t.grid().cell(2, 0).ch, '\0'); // wide trail pad
        assert_eq!(t.grid().cell(3, 0).ch, 'b');
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
        assert_eq!(
            a.cursor_key_application_mode, b.cursor_key_application_mode,
            "DECCKM"
        );
        assert_eq!(
            a.bracketed_paste_mode, b.bracketed_paste_mode,
            "bracketed paste"
        );
        assert_eq!(a.cursor_visible, b.cursor_visible, "cursor visible");
        assert_eq!(a.pending_wrap, b.pending_wrap, "pending_wrap");
        assert_eq!(a.attrs, b.attrs, "current SGR attrs");
        assert_eq!(
            a.saved_cursor.map(|s| (s.col, s.row, s.attrs)),
            b.saved_cursor.map(|s| (s.col, s.row, s.attrs)),
            "saved cursor"
        );
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
        assert!(
            bytes.len() > 1000,
            "wire form suspiciously small: {}",
            bytes.len()
        );
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
        let src = term_with(30, 3, b"\x1b[38;2;200;100;50mRGB\x1b[38;5;82mIDX\x1b[0mDEF");
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
        let src = term_with(20, 5, b"\x1b[31mAB\x1b 7\x1b[?1h\x1b[?2004h\x1b[?25l");
        let bytes = src.serialize_snapshot();
        let mut dst = Terminal::new(20, 5);
        dst.apply_snapshot(&bytes).unwrap();
        assert_terms_equivalent(&src, &dst);
        assert!(dst.cursor_key_application_mode);
        assert!(dst.bracketed_paste_mode);
        assert!(!dst.cursor_visible);
        assert!(dst.saved_cursor.is_some());
    }

    /// v2 regression: feed enough lines to push some into scrollback,
    /// roundtrip the snapshot, verify scrollback_len > 0 on the
    /// restored terminal AND a couple of cell-level samples match.
    /// Without this guard, L3 self-execv silent updates leave panes
    /// with empty scrollback (the "大部分窗口没几行历史" user
    /// report).
    #[test]
    fn snapshot_v2_preserves_scrollback_across_roundtrip() {
        const COLS: u16 = 20;
        const ROWS: u16 = 5;
        let mut src = Terminal::new(COLS, ROWS);
        // 50 lines of distinct text → 45 pushed into scrollback (ROWS=5).
        for i in 0..50u32 {
            src.feed(format!("line {i:03}\r\n").as_bytes());
        }
        let sb_pre = src.grid().scrollback_len();
        assert!(sb_pre > 0, "test setup expected non-empty scrollback");

        let bytes = src.serialize_snapshot();
        let mut dst = Terminal::new(COLS, ROWS);
        dst.apply_snapshot(&bytes).unwrap();

        let sb_post = dst.grid().scrollback_len();
        assert_eq!(
            sb_post, sb_pre,
            "scrollback length should survive snapshot roundtrip (was {sb_pre}, got {sb_post})"
        );
        // Spot-check: pick a known-scrollback line and check its first
        // few cells (scrollback_line(0) is the OLDEST scrollback row).
        let newest = dst
            .grid()
            .scrollback_line(0)
            .expect("scrollback line 0 missing after roundtrip");
        let prefix: String = newest.iter().take(8).map(|c| c.ch).collect();
        assert!(
            prefix.starts_with("line "),
            "newest scrollback line should begin with 'line ' prefix; got {prefix:?}"
        );
    }

    /// F2+4 — when the live scrollback is already non-empty at
    /// apply_snapshot time (the common L3 self-execv case: the file-
    /// or disk-backed scrollback survived the execv with the full
    /// history intact), the snapshot's scrollback section must NOT
    /// be replayed.  Replaying duplicates the tail of the persisted
    /// scrollback into the file once per execv — measured at
    /// ~20 k rows × 9 panes × ~6 installs/day ≈ 1 M dup rows/day
    /// before this gate.
    #[test]
    fn snapshot_v3_fills_execv_gap_without_duplicating_persisted_prefix() {
        // Models the real execv-gap scenario: old L3 had N lines in
        // scrollback, only N-K reached the file before execv (the K
        // tail was buffered in a BufWriter that didn't flush on
        // execv).  Snapshot carries the full N (RAM-ring is truth).
        // New L3 reopens, sees N-K on disk, applies snapshot — must
        // end up with exactly N lines, no duplicates of the on-disk
        // prefix, no missing tail.
        const COLS: u16 = 20;
        const ROWS: u16 = 5;
        // Source = old L3 with all N lines.
        let mut src = Terminal::new(COLS, ROWS);
        let mut canonical: Vec<String> = Vec::new();
        for i in 0..50u32 {
            let s = format!("line {i:03}");
            canonical.push(s.clone());
            src.feed(s.as_bytes());
            src.feed(b"\r\n");
        }
        let src_sb_len = src.grid().scrollback_len();
        assert!(src_sb_len > 0);
        let bytes = src.serialize_snapshot();

        // Destination = new L3 reopens scrollback file that has only
        // the persisted PREFIX (first src_sb_len - GAP lines).  We
        // simulate this by feeding dst the same byte sequence for the
        // first `prefix_len` scrollback lines so its scrollback ring
        // ends up containing exactly the bytes the file would have
        // held.
        const GAP: usize = 10;
        let prefix_len = src_sb_len - GAP;
        let mut dst = Terminal::new(COLS, ROWS);
        // Feed enough lines that prefix_len lines land in scrollback.
        // Relation: after feeding N text lines (each followed by
        // \r\n), scrollback length is N - (ROWS - 1).  The first
        // ROWS-1 LFs land cursor at the bottom row WITHOUT scrolling
        // (cursor < scroll_bot until row ROWS-1 is reached); the
        // ROWS-th LF is the first that triggers a scroll-and-push.
        let lines_to_feed = prefix_len + ROWS as usize - 1;
        for s in canonical.iter().take(lines_to_feed) {
            dst.feed(s.as_bytes());
            dst.feed(b"\r\n");
        }
        let dst_pre_apply = dst.grid().scrollback_len();
        assert_eq!(
            dst_pre_apply, prefix_len,
            "test setup: dst should have exactly the persisted prefix in scrollback"
        );

        // Apply snapshot — should fill the GAP, NOT duplicate prefix.
        dst.apply_snapshot(&bytes).unwrap();

        let dst_post = dst.grid().scrollback_len();
        assert_eq!(
            dst_post, src_sb_len,
            "v3 apply should fill exactly the execv gap (expected {src_sb_len}, got {dst_post})"
        );
        // RFC-004 B14 — content-exact check across the WHOLE ring:
        // every line must be the canonical line for its index (the
        // pre-B14 emitter refilled the gap with copies of the OLDEST
        // lines; a prefix-only assertion never noticed).
        for idx in 0..dst_post {
            let line = dst.grid().scrollback_line(idx).expect("line");
            let txt: String = line
                .iter()
                .take(8)
                .map(|c| c.ch)
                .collect::<String>()
                .trim_end()
                .to_string();
            assert_eq!(
                txt, canonical[idx],
                "scrollback line {idx} content mismatch after gap fill"
            );
        }
    }

    /// RFC-004 C.1 (v4) — alt-screen fold.  A terminal inside
    /// `?1049h` (claudecode-shaped) serializes the MAIN layer as the
    /// snapshot body and appends the alt ring + final alt screen as
    /// the v4 fold section; apply lands the alt content in
    /// scrollback and restores the main screen.  Pre-v4, this
    /// scenario silently lost the whole alt conversation (B13).
    #[test]
    fn snapshot_v4_folds_alt_screen_history_into_scrollback() {
        const COLS: u16 = 20;
        const ROWS: u16 = 5;
        let mut src = Terminal::new(COLS, ROWS);
        // Main-screen prelude: pushes some lines into main scrollback.
        for i in 0..8u32 {
            src.feed(format!("main {i:02}\r\n").as_bytes());
        }
        let main_sb = src.grid().scrollback_len();
        assert!(main_sb > 0);
        // Enter alt (claudecode launch) + emit a conversation that
        // scrolls well past the alt grid.
        src.feed(b"\x1b[?1049h");
        for i in 0..30u32 {
            src.feed(format!("alt line {i:02}\r\n").as_bytes());
        }
        src.feed(b"FINAL ALT SCREEN");
        let bytes = src.serialize_snapshot();

        let mut dst = Terminal::new(COLS, ROWS);
        dst.apply_snapshot(&bytes).unwrap();
        let post = dst.grid().scrollback_len();
        assert!(
            post > main_sb,
            "alt fold must land in scrollback: main={main_sb} post={post}"
        );
        // Newest folded line (idx = len-1; 0 is the OLDEST — B14) =
        // the final visible alt screen content.
        let newest = dst.grid().scrollback_line(post - 1).expect("newest line");
        let text: String = newest.iter().map(|c| c.ch).collect();
        assert!(
            text.starts_with("FINAL ALT SCREEN"),
            "final alt screen row must be the newest history: {text:?}"
        );
        // And the alt conversation lines are present above it.
        let mut found_alt0 = false;
        for idx in 0..post {
            if let Some(l) = dst.grid().scrollback_line(idx) {
                let t: String = l.iter().map(|c| c.ch).collect();
                if t.starts_with("alt line 00") {
                    found_alt0 = true;
                    break;
                }
            }
        }
        assert!(found_alt0, "oldest alt ring line must survive the fold");
        // The restored VISIBLE grid is the main screen (pre-alt), not
        // the alt shadow — a fresh shell draws over the main prompt.
        let row0: String = (0..COLS).map(|c| dst.grid().cell(c, 0).ch).collect();
        assert!(
            row0.starts_with("main "),
            "visible grid must be the restored main screen: {row0:?}"
        );
        // Mode bookkeeping: restored terminal is NOT in alt mode.
        let bytes2 = dst.serialize_snapshot();
        // Serializing the restored terminal must produce alt_present=0
        // (quick structural probe: v4 payload of a non-alt terminal
        // ends with the alt_present byte = 0).
        assert_eq!(bytes2[bytes2.len() - 1], 0, "restored term must not be alt");
    }

    /// RFC-004 C.1 amendment (v5) — LIVE snapshot roundtrip for the
    /// execv handoff: an alt-screen terminal must come back VERBATIM
    /// (current grid = alt, saved_main rebuilt, ring intact, nothing
    /// folded into the File scrollback), so the still-running TUI's
    /// incremental repaints stay seamless.  The fold form here was
    /// the 2026-07-17 "input box vanished after reinstall" field
    /// regression.
    #[test]
    fn snapshot_v5_live_restores_alt_screen_verbatim() {
        const COLS: u16 = 20;
        const ROWS: u16 = 5;
        let mut src = Terminal::new(COLS, ROWS);
        for i in 0..8u32 {
            src.feed(format!("main {i:02}\r\n").as_bytes());
        }
        let main_sb = src.grid().scrollback_len();
        src.feed(b"\x1b[?1049h");
        for i in 0..30u32 {
            src.feed(format!("alt line {i:02}\r\n").as_bytes());
        }
        src.feed(b"INPUT BOX ROW");
        let alt_ring = src.grid().scrollback_len();
        let (acc, acr) = src.grid().cursor();
        let bytes = src.serialize_snapshot_live();

        let mut dst = Terminal::new(COLS, ROWS);
        dst.apply_snapshot(&bytes).unwrap();
        // Current screen is the ALT screen, verbatim.
        let last_row: String = (0..COLS).map(|c| dst.grid().cell(c, acr).ch).collect();
        assert!(
            last_row.starts_with("INPUT BOX ROW"),
            "alt screen must be the visible grid: {last_row:?}"
        );
        assert_eq!(dst.grid().cursor(), (acc, acr), "alt cursor verbatim");
        // Alt ring preserved as the CURRENT grid's scrollback…
        assert_eq!(dst.grid().scrollback_len(), alt_ring, "alt ring intact");
        // …and NOT folded into the main layer: leaving alt shows the
        // main screen with its original scrollback length.
        dst.feed(b"\x1b[?1049l");
        assert_eq!(
            dst.grid().scrollback_len(),
            main_sb,
            "main scrollback must not receive folded alt lines"
        );
        let row0: String = (0..COLS).map(|c| dst.grid().cell(c, 0).ch).collect();
        assert!(
            row0.starts_with("main "),
            "exit-alt must land on the restored main screen: {row0:?}"
        );
    }

    /// RFC-004 C.1 — `serialize_snapshot_capped` with a small tail
    /// cap emits only the newest N main-scrollback lines, and apply's
    /// v3 index arithmetic still dedupes correctly against a disk
    /// that already has everything (periodic-snapshot shape: flush
    /// first, then a snapshot whose tail is a sliver).
    #[test]
    fn snapshot_capped_tail_still_dedupes_on_apply() {
        const COLS: u16 = 20;
        const ROWS: u16 = 5;
        let mut src = Terminal::new(COLS, ROWS);
        for i in 0..40u32 {
            src.feed(format!("line {i:03}\r\n").as_bytes());
        }
        let full = src.grid().scrollback_len();
        let bytes = src.serialize_snapshot_capped(4);
        // Destination already holds the FULL history (the flushed-
        // File periodic case).
        let mut dst = Terminal::new(COLS, ROWS);
        for i in 0..40u32 {
            dst.feed(format!("line {i:03}\r\n").as_bytes());
        }
        assert_eq!(dst.grid().scrollback_len(), full);
        dst.apply_snapshot(&bytes).unwrap();
        assert_eq!(
            dst.grid().scrollback_len(),
            full,
            "capped tail must not duplicate lines the disk already has"
        );
    }

    /// v2 fallback: a synthetic v2 payload arriving at a v3 receiver
    /// still applies — the absence of `start_logical_idx` falls back
    /// to the F2+4 "skip if disk non-empty" heuristic.  Loses the
    /// gap-tail but doesn't crash.  This guards the
    /// `snapshot_v >= 3` branch from regressing into "fall through
    /// to v3 path for v2 payloads" — that branch would mis-read 8
    /// bytes of the line count as a u64 start_idx and corrupt the
    /// rest of the parse.
    #[test]
    fn snapshot_v2_payload_still_apply_able_after_v3_bump() {
        const COLS: u16 = 20;
        const ROWS: u16 = 5;
        let mut src = Terminal::new(COLS, ROWS);
        for i in 0..30u32 {
            src.feed(format!("line {i:03}\r\n").as_bytes());
        }
        let mut bytes = src.serialize_snapshot();
        // Rewrite the version field (bytes [4..8]) from 3 → 2.  Then
        // strip the start_logical_idx field (8 bytes inserted at the
        // start of the trailing scrollback section) so the section
        // matches the v2 layout exactly.  Layout above the
        // scrollback section is version-stable across v2 → v3.
        bytes[4..8].copy_from_slice(&2u32.to_le_bytes());
        // The scrollback section starts at the END of the live grid
        // section.  v3 inserts 8 bytes (start_logical_idx) right at
        // that boundary.  Compute the offset from the wire layout:
        //   4 magic + 4 version + 2 cols + 2 rows + 2 ccol + 2 crow
        // + 2 stop + 2 sbot + 4 modes + 8 generation + ATTRS_BYTES attrs
        // + 1 has_saved_cursor (None → just the flag byte)
        // + (rows*cols * CELL_BYTES) cells
        let header_bytes = 4 + 4 + 2 + 2 + 2 + 2 + 2 + 2 + 4 + 8 + ATTRS_BYTES + 1;
        let cells_bytes = (COLS as usize) * (ROWS as usize) * CELL_BYTES;
        let off = header_bytes + cells_bytes;
        // Splice out the 8-byte start_logical_idx so what remains is
        // valid v2 wire shape.
        let _ = bytes.drain(off..off + 8);

        let mut dst = Terminal::new(COLS, ROWS);
        assert!(
            dst.apply_snapshot(&bytes).is_ok(),
            "synthetic v2 payload must remain apply-able at a v3 receiver"
        );
    }

    /// v2 wrapped flag survival: feed a URL that overflows the row
    /// width so the parser sets wrap, then push it into scrollback,
    /// roundtrip the snapshot, and assert the wrapped flag came back.
    /// Without this, link-scan after silent update would chop long
    /// URLs at the row boundary.
    #[test]
    fn snapshot_v2_preserves_scrollback_wrapped_flag() {
        const COLS: u16 = 20;
        const ROWS: u16 = 4;
        let mut src = Terminal::new(COLS, ROWS);
        // 40 chars of URL forces wrap at col 20, then 12 \n to push
        // those wrapped rows into scrollback.
        src.feed(b"https://example.com/path/extra/segments/end");
        for _ in 0..12 {
            src.feed(b"\r\n");
        }
        // There should be at least one scrollback row with wrapped=true
        // by now (the row that took the second half of the URL).
        let any_wrapped_pre =
            (0..src.grid().scrollback_len()).any(|i| src.grid().scrollback_wrapped(i));
        assert!(
            any_wrapped_pre,
            "test setup expected a wrapped row in scrollback"
        );

        let bytes = src.serialize_snapshot();
        let mut dst = Terminal::new(COLS, ROWS);
        dst.apply_snapshot(&bytes).unwrap();

        let any_wrapped_post =
            (0..dst.grid().scrollback_len()).any(|i| dst.grid().scrollback_wrapped(i));
        assert!(
            any_wrapped_post,
            "wrapped flag should survive snapshot roundtrip (lost after execv would break link scans)"
        );
    }

    /// v2-payload should be apply-able by readers that share the same
    /// MIN_COMPAT.  We can't easily fake a v1-image reader here, but
    /// we DO verify that a v1-shaped (synthetic) payload still applies
    /// via the version-range check — guarding against an accidental
    /// `snapshot_v != SNAPSHOT_VERSION` regression that would re-break
    /// silent updates when only one side has been bumped.
    #[test]
    fn snapshot_v1_still_accepted_by_v2_reader() {
        // Build a real v2 snapshot, then rewrite the version field to 1
        // and truncate the trailing scrollback section.  The reader
        // should accept this as a legacy v1 payload and not return an
        // error.
        let mut src = Terminal::new(20, 5);
        src.feed(b"hello world");
        let mut bytes = src.serialize_snapshot();
        // Bytes 4..8 are the version u32 LE — rewrite to 1.
        bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
        // We don't know exactly where the trailing scrollback bytes
        // start without re-doing the parse, but feeding the full body
        // through with `snapshot_v = 1` already short-circuits the
        // trailing parse; trailing bytes are ignored.
        let mut dst = Terminal::new(20, 5);
        dst.apply_snapshot(&bytes)
            .expect("v1-marked payload must still apply under v2 reader");
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
        assert_eq!(lines[0].0[0].ch, 'A');
        assert_eq!(lines[3].0[0].ch, 'D');
        // Foreground colour survives the wire format.
        assert!(matches!(lines[0].0[0].attrs.fg, Color::Indexed(1)));
        // Each line padded to grid width.
        assert_eq!(lines[0].0.len(), 8);
        // Hard newlines, not autowrap continuations.
        assert!(!lines[0].1);
    }

    #[test]
    fn decode_scrollback_page_legacy_no_wrapped_byte() {
        // L4 shelld 0.2.6 and earlier emit each line as
        // `[cols u32][cells]` with no wrapped byte — decoder must
        // detect that and fall back, reporting wrapped=false for
        // every row so cells line up correctly.
        let attrs = CellAttrs::default();
        let mkcell = |ch: char| Cell { ch, attrs };

        // Hand-build a legacy 2-line, 3-col body.
        let mut body = Vec::new();
        for line_ch in ['X', 'Y'] {
            body.extend_from_slice(&3u32.to_le_bytes());
            for _ in 0..3 {
                body.extend_from_slice(&(line_ch as u32).to_le_bytes());
                body.extend_from_slice(&serialize_attrs(mkcell(line_ch).attrs));
            }
        }
        let lines = Terminal::decode_scrollback_page_body(2, &body).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].0[0].ch, 'X');
        assert_eq!(lines[1].0[2].ch, 'Y');
        assert!(!lines[0].1);
        assert!(!lines[1].1);
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
        let mkline = |ch: char| -> Vec<Cell> { (0..8).map(|_| Cell { ch, attrs }).collect() };
        t.push_historic_line(&mkline('H'), false);
        t.push_historic_line(&mkline('I'), false);
        t.push_historic_line(&mkline('J'), false);
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
        t.feed(b"\x1b[1;3r"); // region [0..=2]
        t.feed(b"\x1b[1;1HAA\r\n"); // row 0 "AA"
        t.feed(b"BB\r\n"); // row 1 "BB"
        t.feed(b"CC\r\n"); // row 2 "CC", then LF triggers region scroll
        t.feed(b"DD"); // ???
        // After the LF after writing "CC", cursor was at row 2 (scroll_bot),
        // region scrolls up. Row 0 "AA" pushed off the region (NOT into
        // scrollback because region != full grid). Row 1 "BB" → row 0,
        // row 2 "CC" → row 1, row 2 blanked. Then "DD" written at row 2.
        assert_eq!(
            t.grid().cell(0, 0).ch,
            'B',
            "row 0 should be BB after scroll"
        );
        assert_eq!(
            t.grid().cell(0, 1).ch,
            'C',
            "row 1 should be CC after scroll"
        );
        assert_eq!(
            t.grid().cell(0, 2).ch,
            'D',
            "row 2 should be DD (new content)"
        );
        // Row 3 (outside region) should be unchanged from initial (blank).
        assert_eq!(
            t.grid().cell(0, 3).ch,
            ' ',
            "row 3 is outside region, unchanged"
        );
    }

    #[test]
    fn il_inserts_blank_lines_at_cursor() {
        let mut t = Terminal::new(10, 5);
        t.feed(b"\x1b[1;1HAA\r\n"); // row 0
        t.feed(b"BB\r\n"); // row 1
        t.feed(b"CC\r\n"); // row 2
        t.feed(b"\x1b[2;1H"); // cursor to row 1
        t.feed(b"\x1b[L"); // IL: insert 1 line at row 1
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
        t.feed(b"\x1b[2;1H"); // cursor to row 1
        t.feed(b"\x1b[M"); // DL: delete 1 line at row 1
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
        assert_eq!(
            t.grid().cell(4, 0).ch,
            ' ',
            "wide-lead under cursor not cleared"
        );
        assert_eq!(t.grid().cell(5, 0).ch, ' ', "wide-trail orphan not cleared");
    }

    /// DSR / CPR: `CSI 6 n` must answer with the cursor's 1-based
    /// position, and `CSI 5 n` with a bare OK.  An app that asks and
    /// is ignored waits forever — readline does this to redraw a
    /// wrapped prompt — and a writer outside the terminal has no way
    /// to tell "consumed my bytes" from "dropped them" without it.
    #[test]
    fn dsr_reports_cursor_position_and_status() {
        let mut t = Terminal::new(20, 5);
        t.feed(b"abc"); // cursor now col 3, row 0
        t.feed(b"\x1b[6n");
        assert_eq!(t.take_response(), b"\x1b[1;4R".to_vec());
        t.feed(b"\x1b[2;7H\x1b[6n"); // move, then ask again
        assert_eq!(t.take_response(), b"\x1b[2;7R".to_vec());
        t.feed(b"\x1b[5n");
        assert_eq!(t.take_response(), b"\x1b[0n".to_vec());
        // An unknown DSR parameter is silently ignored, not answered
        // with a malformed report.
        t.feed(b"\x1b[99n");
        assert!(t.take_response().is_empty());
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
        t.feed(b"\x1b[1;3H"); // cursor (col=2, row=0)
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
        t.feed(b"\x1b[31m"); // red foreground
        t.feed(b"\x1b[2;2H"); // cursor (1, 1)
        t.feed(b"\x1b7"); // save (cursor + attrs)
        t.feed(b"\x1b[34m"); // change to blue
        t.feed(b"\x1b[5;5HX"); // write X in blue at (4, 4)
        t.feed(b"\x1b8"); // restore (cursor → (1,1), attrs → red)
        t.feed(b"Y"); // write Y at (1, 1) in red
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
                t.grid.set_cell(
                    c,
                    r,
                    Cell {
                        ch: marker,
                        ..Default::default()
                    },
                );
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
            assert_eq!(
                t.current_attrs().fg,
                Color::Indexed(idx),
                "code {} -> idx {}",
                code,
                idx
            );
        }
    }

    #[test]
    fn sgr_standard_8_color_bg() {
        for (code, idx) in (40u16..=47).zip(0u8..=7) {
            let mut t = Terminal::new(10, 5);
            t.feed(format!("\x1B[{}m", code).as_bytes());
            assert_eq!(
                t.current_attrs().bg,
                Color::Indexed(idx),
                "code {} -> idx {}",
                code,
                idx
            );
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

    /// 2026-07-29 field report — the omz update banner grew stray
    /// horizontal rules through its links: a `\e[4m` URL wrapping on
    /// the bottom row scrolled a fresh row in while underline was
    /// still on, and the blanks it was filled with carried the
    /// underline.  BCE inherits ONLY the background; every other SGR
    /// bit is a glyph property and blanks have no glyph.
    #[test]
    fn scroll_fill_and_erase_blanks_carry_only_the_background() {
        // Scroll-fill: bottom-row wrap mid-underline.
        let mut t = Terminal::new(20, 2);
        t.feed(b"x\r\ny");
        t.feed(b"\r\x1b[41m\x1b[4m0123456789abcdefghijKLM\x1b[24m\x1b[0m");
        let g = t.grid();
        // Row 1 is the wrapped continuation: "KLM" + fill blanks.
        for c in 3..g.cols() {
            let cell = g.cell(c, 1);
            assert!(
                !cell.attrs.underline,
                "fill blank at col {c} must not be underlined"
            );
            assert!(!cell.attrs.bold && !cell.attrs.reverse && !cell.attrs.dim);
            assert_eq!(cell.attrs.bg, Color::Indexed(1), "…but BCE keeps the bg");
        }
        // The printed glyphs DO keep their underline.
        assert!(
            g.cell(0, 1).attrs.underline,
            "the K is genuinely underlined"
        );

        // Erase path (EL) mid-underline: same contract.
        let mut t = Terminal::new(10, 2);
        t.feed(b"\x1b[42m\x1b[4mab\x1b[K");
        let g = t.grid();
        assert!(g.cell(0, 0).attrs.underline && g.cell(1, 0).attrs.underline);
        for c in 2..10 {
            let cell = g.cell(c, 0);
            assert!(
                !cell.attrs.underline,
                "EL blank at col {c} must not be underlined"
            );
            assert_eq!(cell.attrs.bg, Color::Indexed(2));
        }
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
    fn csi_3_j_clears_scrollback_only() {
        // RFC-004 amendment (2026-07-17): CSI 3 J now clears the
        // PERSISTENT tier too (`clear` + app restart must stay
        // cleared) — File and Memory agree on len()==0 after 3J.
        // This test uses the Memory variant; the File-side
        // persistence contract is pinned in
        // scrollback::tests::clear_truncates_persistent_file.
        // Force Memory by clearing `MARSPOT_SESSION_ID`.
        // The var IS set when the suite runs inside a marspot
        // terminal (L3 exports it to the shell); without this
        // remove_var the test flips to File semantics and fails
        // (safe under nextest's process-per-test isolation).
        // SAFETY: test/example code, single-threaded at this point (state-dir
        // mutations additionally serialized by the suite's state-dir lock).
        unsafe { std::env::remove_var("MARSPOT_SESSION_ID") };
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
        // Force the Memory variant — see csi_3_J test for why the
        // var can be present (suite run inside a marspot terminal).
        // SAFETY: test/example code, single-threaded at this point (state-dir
        // mutations additionally serialized by the suite's state-dir lock).
        unsafe { std::env::remove_var("MARSPOT_SESSION_ID") };
        let mut t = Terminal::new(80, 24);
        // Warm up enough to fully wrap the ring once so all backing
        // pages have been faulted in before we baseline.  The Memory
        // variant has DEFAULT_SCROLLBACK_LINES = 10 000 slots;
        // 40 000 warmup lines covers that with ~4x margin.
        // Without this the test's "baseline" lands mid-fill and
        // the next burst's lazy faults look like a leak.
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
    fn silent_program_gives_the_screen_back_on_its_own() {
        // A `sudo` password prompt: echo off, and not one byte comes
        // back until Enter.  Without a deadline the guesses stay
        // painted — the password, in clear, on screen.
        let mut t = Terminal::new(20, 5);
        for b in b"hunter22" {
            assert!(t.predict_byte(*b));
        }
        assert_eq!(&first_row_text(&t)[..8], "hunter22");
        assert!(t.predictions_pending());

        let deadline = t.predict_deadline();
        assert!(!t.expire_predictions_at(Instant::now()), "not yet");
        assert!(t.expire_predictions_at(Instant::now() + deadline));

        assert_eq!(first_row_text(&t).trim_end(), "");
        assert_eq!(t.grid.cursor(), (0, 0));
        assert_eq!(t.predictions_expired, 8);
        // Nothing arrived, so nothing is decided yet: expiry alone is
        // not evidence the program refuses to echo.
        assert_eq!(t.predict_misses, 0);
    }

    #[test]
    fn late_echo_widens_the_window_instead_of_counting_against_it() {
        // A real link: the echo is slower than the deadline but it
        // does come.  Losing local echo on exactly the connection it
        // was built for would be the wrong lesson to draw.
        let mut t = Terminal::new(20, 5);
        t.predict_byte(b'a');
        let deadline = t.predict_deadline();
        assert!(t.expire_predictions_at(Instant::now() + deadline));
        assert_eq!(first_row_text(&t).trim_end(), "");

        // The echo lands after the guess was taken back.
        t.feed(b"a");
        assert_eq!(&first_row_text(&t)[..1], "a");
        assert_eq!(t.predict_misses, 0, "a late echo is not a miss");
        assert!(t.can_predict(), "the link stays worth predicting on");
    }

    #[test]
    fn the_window_is_learned_from_the_link_not_assumed() {
        // Whatever the round trip turns out to be, the deadline
        // follows it — the floor protects a local pty, the ceiling
        // keeps an unechoed keystroke from lingering.
        let mut t = Terminal::new(20, 5);
        assert_eq!(t.predict_deadline(), Terminal::PREDICT_DEADLINE_MIN);
        for _ in 0..40 {
            t.note_echo_rtt(Duration::from_millis(300));
        }
        assert!(t.predict_deadline() > Duration::from_millis(900));
        for _ in 0..200 {
            t.note_echo_rtt(Duration::from_millis(5000));
        }
        assert_eq!(t.predict_deadline(), Terminal::PREDICT_DEADLINE_MAX);
        for _ in 0..400 {
            t.note_echo_rtt(Duration::from_micros(160));
        }
        assert_eq!(t.predict_deadline(), Terminal::PREDICT_DEADLINE_MIN);
    }

    #[test]
    fn expired_then_something_else_is_a_real_miss() {
        // The password prompt again, now past Enter: what finally
        // arrives is sudo's own output, not the typed bytes.  Three
        // of those and the pane stops guessing.
        let mut t = Terminal::new(20, 5);
        for _ in 0..3 {
            assert!(t.predict_byte(b'x'));
            let deadline = t.predict_deadline();
            assert!(t.expire_predictions_at(Instant::now() + deadline));
            t.feed(b"\r\n");
        }
        assert_eq!(t.predict_misses, 3);
        assert!(!t.can_predict());
        assert!(!t.predict_byte(b'x'));
        assert_eq!(first_row_text(&t).trim_end(), "");
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
    /// 2026-07-28 field report — Homebrew's concurrent-download UI
    /// scrolled forever instead of redrawing in place.  brew moves the
    /// cursor with `\033[0G` + `\033[{n}F` (CPL); marspot's CSI
    /// dispatch had no `F` arm, the sequence was silently dropped, and
    /// every refresh APPENDED its lines.  This test replays brew's
    /// exact redraw shape (Tty.move_cursor_beginning +
    /// move_cursor_up_beginning from download_queue.rb).
    #[test]
    fn brew_style_cpl_redraw_updates_in_place() {
        let mut t = Terminal::new(40, 6);
        t.feed(b"openjdk 10%\r\nqemu 5%");
        // brew's refresh: CR to column 0, up (lines-1) to the first
        // status row, then rewrite both lines with EL after each.
        for pct in [20u32, 30, 40] {
            t.feed(b"\x1b[0G\x1b[1F");
            t.feed(format!("openjdk {pct}%\x1b[K\r\nqemu {pct}%\x1b[K").as_bytes());
        }
        assert_eq!(row_text(&t, 0), "openjdk 40%");
        assert_eq!(row_text(&t, 1), "qemu 40%");
        assert_eq!(row_text(&t, 2), "", "no appended garbage rows");
        assert_eq!(
            t.grid().scrollback_len(),
            0,
            "in-place redraw must not push anything into scrollback"
        );
    }

    /// CNL is CPL's mirror; pin both while we are here.
    #[test]
    fn cnl_moves_down_to_column_zero() {
        let mut t = Terminal::new(20, 5);
        t.feed(b"abc\x1b[2Exyz");
        assert_eq!(row_text(&t, 0), "abc");
        assert_eq!(row_text(&t, 2), "xyz", "down 2 rows, column 0");
        // Clamped at the bottom edge, never scrolls.
        t.feed(b"\x1b[99Eq");
        assert_eq!(row_text(&t, 4), "q");
        assert_eq!(t.grid().scrollback_len(), 0);
    }

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
        assert_eq!(
            logical_text(&t),
            before,
            "wild resize chain must be lossless"
        );
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

#[cfg(test)]
mod osc_tests {
    use super::*;

    #[test]
    fn the_shape_a_program_asks_for_is_recorded_not_dropped() {
        let mut t = Terminal::new(20, 5);
        assert_eq!(t.cursor_shape(), 0);
        t.feed(b"\x1b[5 q");
        assert_eq!(t.cursor_shape(), 5, "bar");
        // What codex actually sends, 119,812 times in one session:
        // shape 0 = "this terminal's default".
        t.feed(b"\x1b[0 q");
        assert_eq!(t.cursor_shape(), 0);
        // `CSI > 0 q` is XTQVERSION, a different sequence that happens
        // to end in the same letter — it must still answer.
        t.feed(b"\x1b[>0q");
        assert!(!t.take_response().is_empty());
        assert_eq!(t.cursor_shape(), 0, "XTQVERSION is not a shape");
    }

    #[test]
    fn a_sequence_with_an_intermediate_is_still_answered() {
        // csi_dispatch returns early for any CSI carrying an
        // intermediate, so an arm written below that point never runs.
        // XTQVERSION sat there dead while the comment beside it said it
        // replied; this is the test that would have caught it.
        let mut t = Terminal::new(20, 5);
        t.feed(b"\x1b[>0q");
        assert_eq!(
            String::from_utf8(t.take_response()).unwrap(),
            "\x1bP>|marspot\x1b\\",
            "XTQVERSION must answer"
        );
    }

    #[test]
    fn a_color_query_gets_an_answer() {
        // An unanswered one is how a TUI ends up guessing whether it
        // is drawing on a dark terminal.
        let mut t = Terminal::new(20, 5);
        t.feed(b"\x1b]11;?\x07");
        let r = String::from_utf8(t.take_response()).unwrap();
        assert_eq!(r, format!("\x1b]11;{}\x07", crate::palette::xterm_rgb(crate::palette::BG)));

        t.feed(b"\x1b]10;?\x07");
        let r = String::from_utf8(t.take_response()).unwrap();
        assert!(r.starts_with("\x1b]10;rgb:"), "the number asked is the number answered: {r:?}");
    }

    #[test]
    fn setting_a_color_is_not_a_query_and_gets_no_reply() {
        let mut t = Terminal::new(20, 5);
        t.feed(b"\x1b]11;rgb:0000/0000/0000\x07");
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn a_program_that_says_what_it_is_is_not_thrown_away() {
        let mut t = Terminal::new(20, 5);
        assert_eq!(t.osc_title(), "");
        t.feed(b"\x1b]0;codex \xe2\xa0\x99 building\x07");
        assert_eq!(t.osc_title(), "codex ⠙ building");
        // OSC 2 is the same title by another number.
        t.feed(b"\x1b]2;second\x07");
        assert_eq!(t.osc_title(), "second");
        // None of it reaches the screen.
        let row: String = (0..20).map(|c| t.grid().cell(c, 0).ch).collect();
        assert_eq!(row.trim_end_matches('\0').trim(), "");
    }

    #[test]
    fn a_title_is_a_label_not_a_buffer() {
        let mut t = Terminal::new(20, 5);
        let mut b = b"\x1b]0;".to_vec();
        b.extend(std::iter::repeat(b'x').take(10_000));
        b.push(0x07);
        t.feed(&b);
        assert_eq!(t.osc_title().chars().count(), 256);
    }
}

#[cfg(test)]
mod sync_output_tests {
    use super::Terminal;

    /// DEC 2026 opens and closes a batch.  codex brackets every frame
    /// with it; treating it as a no-op is what let half-drawn screens
    /// reach the display.
    #[test]
    fn dec_2026_opens_and_closes_a_synchronized_update() {
        let mut t = Terminal::new(20, 4);
        assert!(!t.sync_output_active());
        t.feed(b"\x1b[?2026h");
        assert!(t.sync_output_active());
        t.feed(b"hello");
        assert!(
            t.sync_output_active(),
            "content inside the batch keeps it open"
        );
        t.feed(b"\x1b[?2026l");
        assert!(!t.sync_output_active());
    }

    /// A program that dies mid-update must not hold the next one's
    /// first frame hostage.
    #[test]
    fn a_process_handover_clears_a_dangling_update() {
        let mut t = Terminal::new(20, 4);
        t.feed(b"\x1b[?2026h");
        assert!(t.sync_output_active());
        t.reset_process_owned_modes();
        assert!(!t.sync_output_active());
    }
}

#[cfg(test)]
mod alt_scroll_tests {
    use super::Terminal;

    /// codex opens its transcript with `?1049h · ?1007h` and closes it
    /// with `?1007l · ?1049l`.  That pair is the program stating, in
    /// the standard way, that the wheel is the arrow keys here — the
    /// thing three rounds of reading its headings off the screen were
    /// trying to work out.
    #[test]
    fn the_transcript_sequence_turns_the_wheel_into_arrow_keys() {
        let mut t = Terminal::new(20, 4);
        assert!(!t.alt_scroll_mode());
        t.feed(b"\x1b[?1049h\x1b[?1007h");
        assert!(t.alt_scroll_mode());
        t.feed(b"\x1b[?1007l\x1b[?1049l");
        assert!(!t.alt_scroll_mode());
    }

    /// The mode belongs to a full-screen view.  A program that set it
    /// and left the alternate screen without clearing it must not keep
    /// the wheel away from the scrollback the user can actually see.
    #[test]
    fn it_does_not_apply_on_the_main_screen() {
        let mut t = Terminal::new(20, 4);
        t.feed(b"\x1b[?1049h\x1b[?1007h");
        assert!(t.alt_scroll_mode());
        t.feed(b"\x1b[?1049l");
        assert!(
            !t.alt_scroll_mode(),
            "no alternate screen, no claim on the wheel"
        );
    }

    /// It has to survive an image swap.  The program said it once, on
    /// entering a view it is still in, and will not say it again — and
    /// a pane whose plugin enters with a TOGGLE would then press that
    /// toggle on an open view and shut it.
    #[test]
    fn it_survives_an_execv() {
        let mut before = Terminal::new(20, 4);
        before.feed(b"\x1b[?1049h\x1b[?1007h");
        assert!(before.in_alt_screen() && before.alt_scroll_mode());
        let snap = before.serialize_snapshot_live();
        let mut after = Terminal::new(20, 4);
        after.apply_snapshot(&snap).expect("snapshot applies");
        assert!(
            after.alt_scroll_mode(),
            "the wheel must still reach the program after the swap"
        );
    }

    /// A dead program's claim on the wheel dies with it.
    #[test]
    fn a_process_handover_gives_the_wheel_back() {
        let mut t = Terminal::new(20, 4);
        t.feed(b"\x1b[?1049h\x1b[?1007h");
        t.reset_process_owned_modes();
        assert!(!t.alt_scroll_mode());
    }
}

#[cfg(test)]
mod alt_screen_across_execv_tests {
    use super::Terminal;

    /// A pane inside a full-screen program must still know it is
    /// there after an image swap.
    ///
    /// Two snapshot forms exist and only one is right here: the fold
    /// form deliberately folds an alt screen into main (it is for
    /// persistence, where the shell's history is what matters), while
    /// `serialize_snapshot_live` is the execv handoff and carries the
    /// alt screen verbatim.  Reading the wrong one reports the state
    /// as lost, which is how it nearly got "fixed" — everything keyed
    /// on `FLAG_ALT_SCREEN` would then be keyed on an artefact.
    #[test]
    fn the_alternate_screen_survives_a_snapshot() {
        let mut before = Terminal::new(20, 4);
        before.feed(b"\x1b[?1049hinside the TUI");
        assert!(before.in_alt_screen());

        let mut after = Terminal::new(20, 4);
        after
            .apply_snapshot(&before.serialize_snapshot_live())
            .unwrap();
        assert!(
            after.in_alt_screen(),
            "the program is still in its own screen; the swap was ours, not its"
        );
    }

    /// A pane that was NOT in one must not be told it was.
    #[test]
    fn an_ordinary_pane_is_not_moved_into_one() {
        let mut before = Terminal::new(20, 4);
        before.feed(b"$ ls");
        let mut after = Terminal::new(20, 4);
        after
            .apply_snapshot(&before.serialize_snapshot_live())
            .unwrap();
        assert!(!after.in_alt_screen());
    }

    /// Leaving works, and lands on a blank main screen — the honest
    /// outcome, since the main grid was never in the snapshot.  The
    /// alternative, staying put, leaves the program's own painting
    /// behind as the shell's history.
    #[test]
    fn leaving_after_a_swap_does_not_keep_the_programs_painting() {
        let mut before = Terminal::new(20, 4);
        before.feed(b"\x1b[?1049hTUI PAINTED THIS");
        let mut after = Terminal::new(20, 4);
        after
            .apply_snapshot(&before.serialize_snapshot_live())
            .unwrap();

        let painted: String = (0..20).map(|c| after.grid().cell(c, 0).ch).collect();
        assert!(
            painted.contains("TUI"),
            "the alternate screen is what we restored"
        );

        after.feed(b"\x1b[?1049l");
        assert!(!after.in_alt_screen());
        let main: String = (0..20).map(|c| after.grid().cell(c, 0).ch).collect();
        assert!(
            !main.contains("TUI"),
            "the program's screen must not become the shell's: {main:?}"
        );
    }
}

#[cfg(test)]
mod u_tag_tests {
    use super::Terminal;

    /// A pane whose plugin has said its program prints markup it does
    /// not render.  That declaration is the only thing that turns this
    /// on — see `MsgType::PaneRenderMarkup`.
    fn declared(cols: u16, rows: u16) -> Terminal {
        let mut t = Terminal::new(cols, rows);
        t.set_render_markup(true);
        t
    }

    fn row(t: &Terminal, r: u16) -> String {
        (0..t.grid().cols())
            .map(|c| t.grid().cell(c, r).ch)
            .collect()
    }
    fn under(t: &Terminal, c: u16) -> bool {
        t.grid().cell(c, 0).attrs.underline
    }

    /// A matched pair styles what it wraps and nothing else.
    #[test]
    fn a_matched_pair_underlines_what_it_wraps() {
        let mut t = declared(20, 2);
        t.feed(b"a<u>bc</u>d");
        assert_eq!(
            row(&t, 0).trim_end(),
            "abcd",
            "the tags are styling, not text"
        );
        assert!(!under(&t, 0), "before");
        assert!(under(&t, 1) && under(&t, 2), "inside");
        assert!(!under(&t, 3), "after");
    }

    /// An UNMATCHED opening tag styles nothing.  It is printed, and so
    /// is everything after it — text that merely mentions the tag is
    /// most of any conversation about this feature, and underlining
    /// the rest of the pane on account of one was the regression this
    /// pins (2026-09-06).
    #[test]
    fn an_unmatched_open_tag_is_just_text() {
        let mut t = declared(40, 2);
        t.feed(b"see <u> in the docs");
        t.feed(b"\r\n");
        assert_eq!(row(&t, 0).trim_end(), "see <u> in the docs");
        for c in 0..20 {
            assert!(!under(&t, c), "column {c} must not be underlined");
        }
    }

    /// The tags take no width once matched: they are markup, and a
    /// terminal that left seven blank cells behind would push the
    /// rest of the line out of the place the program laid it out for.
    #[test]
    fn matched_tags_occupy_no_cells() {
        let mut t = declared(20, 2);
        t.feed(b"<u>ab</u>!");
        assert_eq!(row(&t, 0).trim_end(), "ab!");
        assert_eq!(t.grid().cursor().0, 3, "cursor sits right after the text");
        for c in 3..20 {
            assert_eq!(t.grid().cell(c, 0).ch, ' ', "column {c} must be untouched");
        }
    }

    /// A closing tag with nothing open is text too.
    #[test]
    fn a_stray_closing_tag_is_just_text() {
        let mut t = declared(30, 2);
        t.feed(b"a</u>b");
        assert_eq!(row(&t, 0).trim_end(), "a</u>b");
        assert!(!under(&t, 0) && !under(&t, 5));
    }

    /// A span cannot cross a line: the newline ends it, and the tag
    /// comes back as text.
    #[test]
    fn a_span_broken_by_a_newline_is_not_styled() {
        let mut t = declared(30, 3);
        t.feed(b"x<u>abc\r\ndef</u>");
        assert_eq!(row(&t, 0).trim_end(), "x<u>abc");
        for c in 0..7 {
            assert!(!under(&t, c), "column {c}");
        }
    }

    /// A tag split across two PTY reads is still one tag: the state is
    /// the terminal's, not the chunk's.
    #[test]
    fn a_tag_split_across_reads_still_counts() {
        let mut t = declared(20, 2);
        t.feed(b"a<");
        t.feed(b"u>b");
        t.feed(b"</u>c");
        assert_eq!(row(&t, 0).trim_end(), "abc");
        assert!(under(&t, 1) && !under(&t, 2));
    }

    /// Text that merely starts with `<` comes out untouched.
    #[test]
    fn text_that_is_not_a_tag_is_printed_verbatim() {
        for text in ["a<b>c", "a<ub>c", "a</x>c", "a<uu>c", "if a<b then"] {
            let mut t = declared(30, 2);
            t.feed(text.as_bytes());
            assert_eq!(row(&t, 0).trim_end(), text, "{text:?} was altered");
        }
    }

    /// A dangling `<` at the end of a line must still appear.
    #[test]
    fn a_dangling_open_bracket_is_not_eaten() {
        let mut t = declared(20, 2);
        t.feed(b"a<");
        t.feed(b"\r\n");
        assert!(row(&t, 0).starts_with("a<"), "got {:?}", row(&t, 0));
    }

    /// An escape sequence inside a span ends it — the span's text is
    /// printed with its tag, which is what the pane showed before the
    /// feature existed.
    #[test]
    fn an_escape_sequence_inside_a_span_falls_back_to_text() {
        let mut t = declared(40, 2);
        t.feed(b"<u>ab\x1b[31mcd</u>");
        assert!(row(&t, 0).starts_with("<u>ab"), "got {:?}", row(&t, 0));
        assert!(!under(&t, 3));
    }

    /// An UNDECLARED pane leaves the characters alone.  This is the
    /// default, and it is what makes the feature safe to have at all:
    /// the panes where people discuss markup are not the panes a
    /// plugin declared.
    #[test]
    fn an_undeclared_pane_shows_the_tags() {
        let mut t = Terminal::new(30, 2);
        t.feed(b"a<u>bc</u>d");
        assert_eq!(row(&t, 0).trim_end(), "a<u>bc</u>d");
        assert!(!under(&t, 1));
    }
}

#[cfg(test)]
mod attrs_handover_tests {
    use super::Terminal;

    /// A style the dead program left on must not become the new
    /// shell's.  Modes are already cleared here; a colour or an
    /// underline is the same kind of debt, and worse in one way — a
    /// mode has a prompt that usually turns it off, while a style can
    /// ride every new cell for hours.
    #[test]
    fn a_style_left_on_by_a_dead_program_does_not_reach_the_new_shell() {
        let mut t = Terminal::new(20, 2);
        t.feed(b"\x1b[4;31munderlined red");
        assert!(t.grid().cell(0, 0).attrs.underline);

        t.reset_process_owned_modes();
        t.feed(b"\r\n$ ");
        assert!(
            !t.grid().cell(0, 1).attrs.underline,
            "the new shell's prompt wears its own look"
        );
    }
}

#[cfg(test)]
mod predict_learning_tests {
    use super::Terminal;

    /// A program that echoes at the cursor keeps the feature.
    /// Measured at a real zsh prompt: 16 keystrokes, 15 hits.
    #[test]
    fn a_program_that_echoes_keeps_predicting() {
        let mut t = Terminal::new(40, 4);
        for c in b"echo hi" {
            assert!(t.predict_byte(*c), "prediction offered");
            t.feed(&[*c]); // the echo confirms it
        }
        assert!(t.can_predict(), "nothing here has gone wrong");
        assert_eq!(t.predictions_miss, 0);
    }

    /// A program that draws its own input somewhere else never
    /// confirms, and the terminal stops guessing rather than putting a
    /// character where the user is not looking.  Measured inside
    /// codex: 23 keystrokes, 0 hits.
    #[test]
    fn a_program_that_never_echoes_is_given_up_on() {
        let mut t = Terminal::new(40, 4);
        for _ in 0..Terminal::PREDICT_GIVE_UP_AFTER {
            assert!(t.can_predict());
            assert!(t.predict_byte(b'a'));
            t.feed(b"X"); // a repaint, not an echo
        }
        assert!(
            !t.can_predict(),
            "three wasted keystrokes is the whole budget for finding this out"
        );
    }

    /// Giving up is not forever: the pane a program was quit in
    /// becomes a shell again, and nothing announces that.
    #[test]
    fn one_keystroke_in_a_while_asks_again() {
        let mut t = Terminal::new(40, 4);
        for _ in 0..Terminal::PREDICT_GIVE_UP_AFTER {
            t.predict_byte(b'a');
            t.feed(b"X");
        }
        assert!(!t.can_predict());
        // Each declined keystroke counts toward the next probe, and
        // the one that takes the count TO the threshold is itself
        // declined — the probe is the keystroke after it.
        for _ in 0..Terminal::PREDICT_PROBE_EVERY {
            assert!(!t.predict_byte(b'b'), "still declined");
        }
        assert!(t.predict_byte(b'b'), "one probe is let through");
        t.feed(b"b"); // this program echoes now
        assert!(t.can_predict(), "and a hit hands the feature back");
    }

    /// The probe is spent whether or not it teaches anything, so a
    /// program that still does not echo costs one stray character per
    /// PREDICT_PROBE_EVERY keystrokes and not one per keystroke.
    #[test]
    fn a_spent_probe_does_not_repeat_immediately() {
        let mut t = Terminal::new(40, 4);
        for _ in 0..Terminal::PREDICT_GIVE_UP_AFTER {
            t.predict_byte(b'a');
            t.feed(b"X");
        }
        for _ in 0..=Terminal::PREDICT_PROBE_EVERY {
            t.predict_byte(b'b'); // the last of these is the probe
        }
        t.feed(b"X"); // the probe missed too
        assert!(
            !t.predict_byte(b'c'),
            "the next keystroke is not another probe"
        );
    }
}
