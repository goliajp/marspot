//! Building a `Terminal`: choosing file-backed or in-RAM scrollback
//! and the starting state of every field.

use super::{KittyKeyboardStack, MouseTrackingMode, Osc8, ScreenModes, Terminal};
use crate::grid::{CellAttrs, DEFAULT_SCROLLBACK_LINES, Grid};
use crate::parser::Parser;
use crate::scrollback::Scrollback;
use std::collections::VecDeque;

/// F2 — file-backed scrollback is on whenever a session id is present
/// (L3 `marspot-session` process).  Benches / `--snapshot` / tests don't
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

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        // F2 — file-backed scrollback wins inside an L3 session
        // (MARSPOT_SESSION_ID set).  Corrupt pairs are quarantined +
        // recreated inside `Scrollback::file` (RFC-004 A.4), so the
        // only errors reaching here are environmental (permissions,
        // ENOSPC) — those fall back to in-RAM Memory so the session
        // still boots, and the failure lands in marspot.log (the old
        // eprintln went to /dev/null: L3's stderr is nulled).
        // Tests / benches / --snapshot don't set the env and so go
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
            kitty_keyboard: KittyKeyboardStack::default(),
            parser: Parser::new(),
            attrs: CellAttrs::default(),
            osc8: Osc8::default(),
            saved_cursor: None,
            parked_saved_cursor: None,
            scroll_top: 0,
            scroll_bot: rows.saturating_sub(1),
            pending_response: Vec::new(),
            response_window: VecDeque::new(),
            response_burst_since: None,
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
            tabs: crate::tabs::TabStops::new(cols),
            osc_clipboard: None,
            bell: false,
            g0_graphics: false,
            origin_mode: false,
            insert_mode: false,
            screen_modes: ScreenModes::default(),
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
}
