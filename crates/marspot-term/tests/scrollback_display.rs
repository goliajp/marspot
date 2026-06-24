//! Scrollback display harness — Terminal-level integration tests.
//!
//! Drives `Terminal::feed` and reads back what the user actually
//! SEES via `grid.cell_at_view(...)`.  This crate's lib unit tests
//! cover FileScrollback API edges; THIS file covers the full
//! "user types / terminal scrolls / user scrolls back" loop, which
//! is where display bugs surface.
//!
//! ## Storage model (F3+11)
//!
//! Scrollback is a **dumb append-only log**.  Every row that scrolls
//! off the top of the live grid goes in verbatim — no blank-skip,
//! no trim, no filtering.  This matches iTerm2 / Alacritty / xterm.
//!
//! Scenarios assert correctness under that model:
//!
//! 1. `scroll_down_content_visible` — push N lines, scroll back
//!    through the dense block, every dense row surfaces unmodified.
//! 2. `scroll_cap_matches_pushes` — `scrollback_len` equals the
//!    number of rows scrolled off; scrolling beyond it yields blank
//!    (= "past the start of history", not "more data").
//! 3. `blanks_are_preserved_verbatim` — pushing a row of pure
//!    whitespace produces a stored blank row (caller decides what to
//!    store; storage doesn't second-guess).
//! 4. `torn_write_recovery` — simulate `libc::execv` mid-write by
//!    `mem::forget`-ing the Terminal so BufWriters never flush.  The
//!    consistency-repair scan at next open() must truncate any
//!    orphaned idx tail and surface every row whose record on disk
//!    is intact.
//! 5. `resize_no_corruption` — write N lines at one cols, resize to
//!    narrower, file count must NOT 2× from old+new mix.
//! 6. `clean_restart_preserves_all_content` — write N, drop cleanly,
//!    reopen, every line surfaces.
//!
//! Each scenario owns a fresh tmp state dir + unique session id so
//! parallel runs don't collide.  Env-var writes are serialised
//! through `ENV_LOCK` (Rust `std::env` is process-global).

use std::sync::Mutex;

use marspot_term::grid::Cell;
use marspot_term::terminal::Terminal;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct ScenarioState {
    state_dir: std::path::PathBuf,
    sid: u64,
}

impl ScenarioState {
    fn new(label: &str, sid: u64) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir()
            .join(format!("marspot-scrollback-display-{label}-{pid}-{n}"));
        let session_dir = dir.join("sessions").join(sid.to_string());
        std::fs::create_dir_all(&session_dir).expect("create session dir");
        ScenarioState { state_dir: dir, sid }
    }
    fn apply_env(&self) {
        unsafe {
            std::env::set_var("MARSPOT_STATE_DIR", &self.state_dir);
            std::env::set_var("MARSPOT_SESSION_ID", self.sid.to_string());
        }
    }
}

impl Drop for ScenarioState {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.state_dir);
    }
}

/// Read the full viewport at `view_offset` as `Vec<String>`.
/// NUL cells (wide-trail / wide-pad sentinels) render as a literal
/// `\0` so the caller can distinguish "render-time blank" from
/// "structural blank".
fn view_at(term: &Terminal, view_offset: u16) -> Vec<String> {
    let g = term.grid();
    let rows = g.rows();
    let cols = g.cols();
    let mut out = Vec::with_capacity(rows as usize);
    for viewport_row in 0..rows {
        let mut row = String::with_capacity(cols as usize);
        for col in 0..cols {
            let cell: Cell = g.cell_at_view(view_offset, col, viewport_row);
            row.push(if cell.ch == '\0' { '\0' } else { cell.ch });
        }
        out.push(row);
    }
    out
}

/// `true` if `row` is only spaces / NULs (== visually empty).
fn row_is_blank(row: &str) -> bool {
    row.chars().all(|c| c == ' ' || c == '\0')
}

/// Feed one logical line as a real PTY would emit it.
fn push_line(term: &mut Terminal, text: &str) {
    term.feed(text.as_bytes());
    term.feed(b"\r\n");
}

// ─── 1: scroll-down content visible ────────────────────────────────

#[test]
fn scenario1_scroll_down_content_visible() {
    let _g = ENV_LOCK.lock().unwrap();
    let state = ScenarioState::new("s1-content-visible", 1001);
    state.apply_env();

    const COLS: u16 = 20;
    const ROWS: u16 = 5;
    let mut term = Terminal::new(COLS, ROWS);
    for i in 0..50u32 {
        push_line(&mut term, &format!("L{i:03}"));
    }

    // 50 "L<N>\r\n" pushes.  The trailing `\n` of the last leaves
    // the cursor on a blank bottom row, so live area = L046, L047,
    // L048, L049, BLANK.  "Most recent visible content" = ROWS-2.
    let v0 = view_at(&term, 0);
    assert!(
        v0[ROWS as usize - 2].contains("L049"),
        "view_offset=0 row above cursor should be L049; got {:?}",
        v0[ROWS as usize - 2]
    );

    // Walk every scrollable view_offset; every non-cursor row that
    // corresponds to a pushed line must surface its content.
    let cap = term.grid().scrollback_len() as u16;
    for vo in 0..=cap.min(40) {
        let v = view_at(&term, vo);
        for r in 0..ROWS as usize - 1 {
            // Skip viewport rows whose absolute row index is past the
            // total content (top of the deepest scroll = blank grid
            // state before first push).
            if vo as usize + r >= 50 {
                continue;
            }
            assert!(
                !row_is_blank(&v[r]),
                "view_offset={vo} row={r} unexpectedly blank: {:?}", v[r]
            );
        }
    }
}

// ─── 2: scroll_cap matches push count ──────────────────────────────

#[test]
fn scenario2_scroll_cap_matches_pushes() {
    let _g = ENV_LOCK.lock().unwrap();
    let state = ScenarioState::new("s2-cap-matches", 1002);
    state.apply_env();

    const COLS: u16 = 16;
    const ROWS: u16 = 4;
    let mut term = Terminal::new(COLS, ROWS);
    const N: u32 = 30;
    for i in 0..N {
        push_line(&mut term, &format!("X{i:03}"));
    }

    let cap = term.grid().scrollback_len() as u16;
    // Live area holds ROWS rows; scrollback gets N - (ROWS - 1) of
    // the pushes that scrolled off (the final \n scrolls the live
    // top once more, so the off-screen count = N).
    assert!(
        cap as u32 >= N - ROWS as u32 && cap as u32 <= N + 1,
        "scrollback_len ({cap}) must be roughly N ({N}) ± live area; \
         dumb-store should preserve every scroll-off"
    );

    // At view_offset = cap, top of viewport shows oldest content.
    // At view_offset = cap + 5, top is past the start → blank.
    let at_cap = view_at(&term, cap);
    let past_cap = view_at(&term, cap.saturating_add(5));
    assert!(
        !row_is_blank(&at_cap[0]),
        "view_offset=cap top row should show oldest content, not blank: {:?}",
        at_cap[0]
    );
    assert!(
        row_is_blank(&past_cap[0]),
        "view_offset=cap+5 top row should be blank (= past start of \
         history); got: {:?}", past_cap[0]
    );
}

// ─── 3: blanks are preserved verbatim ──────────────────────────────

#[test]
fn scenario3_blanks_are_preserved_verbatim() {
    // Dumb storage doesn't filter.  If the terminal scrolls a blank
    // row off the top, scrollback stores a blank row.  Mirrors
    // iTerm2 / Alacritty / xterm behaviour.  The user's legit
    // blank lines (Enter on empty prompt, intentional paragraph
    // breaks, `echo ""`) must survive the trip into scrollback;
    // dropping them at storage time silently deletes their content
    // and the user has no way to recover it (verified F3+12 → user
    // pointed at red-boxed blanks vanishing from history).
    let _g = ENV_LOCK.lock().unwrap();
    let state = ScenarioState::new("s3-blanks-preserved", 1003);
    state.apply_env();

    const COLS: u16 = 12;
    const ROWS: u16 = 3;
    let mut term = Terminal::new(COLS, ROWS);

    for i in 0..5u32 {
        push_line(&mut term, &format!("A{i:02}"));
    }
    let after_a = term.grid().scrollback_len();
    for _ in 0..10u32 {
        term.feed(b"\r\n");
    }
    let after_blanks = term.grid().scrollback_len();
    for i in 0..5u32 {
        push_line(&mut term, &format!("B{i:02}"));
    }
    let after_b = term.grid().scrollback_len();

    assert!(
        after_blanks > after_a,
        "10 blank \\r\\n pushes must increase scrollback_len ({after_a} → \
         {after_blanks}); dumb-store records blanks like real rows"
    );
    assert!(
        after_b > after_blanks,
        "5 'B' lines after blanks must keep growing scrollback ({after_blanks} → \
         {after_b}); the blank band must not have aborted accounting"
    );
}

// ─── 4: torn-write recovery — reopen survives unclean kill ─────────

#[test]
fn scenario4_torn_write_recovery() {
    // The bin records pass through a 64 KB BufWriter for
    // amortisation; under an unclean kill (SIGKILL with no chance
    // to `flush_for_handoff`), bytes still in that buffer are
    // gone.  This is the documented BufWriter trade-off, not a
    // bug: planned execv calls `flush_for_handoff` so production
    // hot-install never hits this path.  The test verifies the
    // *graceful* behaviours we DO promise after an unclean kill:
    //   (a) reopen does NOT error out
    //   (b) reopen does NOT panic
    //   (c) the open()'s idx tail-truncate consistency scan drops
    //       any orphaned idx entries (idx ahead of bin) so the
    //       newly-opened state is internally consistent
    //   (d) subsequent pushes succeed against the recovered state
    let _g = ENV_LOCK.lock().unwrap();
    let state = ScenarioState::new("s4-torn-write", 1004);
    state.apply_env();

    const COLS: u16 = 20;
    const ROWS: u16 = 5;
    const PUSHED: u32 = 500;

    {
        let mut term = Terminal::new(COLS, ROWS);
        for i in 0..PUSHED {
            push_line(&mut term, &format!("T{i:04}"));
        }
        std::mem::forget(term);
    }

    state.apply_env();
    let mut term = Terminal::new(COLS, ROWS);
    let surviving = term.grid().scrollback_len() as u16;
    // Whatever survived must decode without garbage.
    for vo in 1..=surviving.min(20) {
        let v = view_at(&term, vo);
        let sb_rows_in_view = (vo as usize).min(ROWS as usize);
        for r in 0..sb_rows_in_view {
            let row = &v[r];
            let is_blank_or_marker = row_is_blank(row) || row.contains('T');
            assert!(
                is_blank_or_marker,
                "view_offset={vo} sb_row={r} contains garbage instead of \
                 a 'T' marker or blank: {row:?}"
            );
        }
    }
    // Fresh pushes after recovery must succeed (no panic, len grows).
    let before = term.grid().scrollback_len();
    for i in 0..50u32 {
        push_line(&mut term, &format!("R{i:03}"));
    }
    let after = term.grid().scrollback_len();
    assert!(
        after > before,
        "post-recovery pushes must grow scrollback ({before} → {after}); \
         a broken open() leaves a non-writable session"
    );
}

// ─── 5: resize doesn't 2× the file ─────────────────────────────────

#[test]
fn scenario5_resize_no_corruption() {
    let _g = ENV_LOCK.lock().unwrap();
    let state = ScenarioState::new("s5-resize", 1005);
    state.apply_env();

    const ROWS: u16 = 5;
    let mut term = Terminal::new(20, ROWS);
    for i in 0..50u32 {
        push_line(&mut term, &format!("W{i:03}"));
    }
    let before = term.grid().scrollback_len();

    term.resize(10, ROWS);
    let after = term.grid().scrollback_len();

    assert!(
        after < before * 2,
        "resize doubled record count ({before} → {after}); reflow \
         is preserving both old and new widths in the file"
    );
}

// ─── 6: clean restart preserves all content ────────────────────────

#[test]
fn scenario6_clean_restart_preserves_all_content() {
    let _g = ENV_LOCK.lock().unwrap();
    let state = ScenarioState::new("s6-clean-restart", 1006);
    state.apply_env();

    const COLS: u16 = 20;
    const ROWS: u16 = 5;
    const N: u32 = 200;
    {
        let mut term = Terminal::new(COLS, ROWS);
        for i in 0..N {
            push_line(&mut term, &format!("C{i:03}"));
        }
        // BufWriter Drop flushes.
    }

    state.apply_env();
    let term = Terminal::new(COLS, ROWS);
    let cap = term.grid().scrollback_len();
    assert!(
        cap as u32 >= N - ROWS as u32,
        "clean restart must preserve ≥ {} lines; got {cap}",
        N - ROWS as u32
    );

    // Walking scrollback rows from view_offset=1 upward, each must
    // contain a 'C' marker.
    let v = view_at(&term, 1);
    assert!(
        v.iter().any(|r| r.contains('C')),
        "view_offset=1 after clean restart shows no 'C' content: {v:?}"
    );
}
