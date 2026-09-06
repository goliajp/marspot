//! Disaster-recovery tool: rebuild a session's on-disk scrollback
//! (`scrollback.bin` + `scrollback.idx`) by replaying its append-only
//! `bytelog` through the real parser → grid → FileScrollback pipeline.
//!
//! Born 2026-07-03: a test run inside a marspot pane inherited
//! MARSPOT_SESSION_ID and overwrote session 347's scrollback.bin with
//! test residue.  The bytelog (raw PTY bytes, capped at 100 MiB) was
//! untouched, so the full history could be regenerated from it.  The
//! env-leak itself is fixed (PtyConfig::env_remove_prefixes); this
//! tool remains for the next "the .bin is gone but the bytelog
//! survived" incident.
//!
//! Usage:
//!   cargo run --release -p marspot-term --example \
//!     rebuild_scrollback_from_bytelog -- \
//!     <bytelog-file> <staging-state-dir> <session-id> <cols> <rows> \
//!
//! `<bytelog-file>` names the LIVE segment; if a retired `<file>.1`
//! sits next to it (the log rotates at 50 MiB) it is replayed first,
//! so the recovered history spans both.
//!     [dump-text-path] [state-bin-out]
//!
//! With the optional 7th arg, ALSO writes the replayed terminal's
//! `serialize_snapshot()` (v4 — alt ring + final alt screen folded)
//! to that path.  Drop it into `sessions/<id>/state.bin` alongside
//! the rebuilt scrollback pair and the next L3 cold boot applies it:
//! the pane comes back showing the pre-crash screen with the folded
//! content as scrollable history (RFC-004 crash recovery).
//!
//! Output lands in `<staging-state-dir>/sessions/<id>/scrollback.{bin,idx}`.
//! With the optional 6th arg, ALSO writes a human-readable UTF-8 dump
//! (every scrollback line + the final visible grid) to that path —
//! for TUI-heavy sessions (claudecode, vim) almost nothing scrolls
//! off-grid, so the final grid is where the recoverable content
//! actually lives.
//! ALWAYS point this at a staging dir, never the live state dir — the
//! live L3 owns its files.  Swap protocol (proven on 347): SIGSTOP the
//! L3 → freeze-copy its bytelog → replay here → rename the rebuilt
//! pair into place → land any commit + `bin/install-local.sh` (new
//! fingerprint → SIGTERM fan-out queues on the stopped L3) → SIGCONT
//! → the L3 execv's and reopens the rebuilt files.  Wrap positions may
//! differ from the original where the pane was resized mid-history
//! (the replay runs at one fixed width); content is complete.

use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if !(6..=8).contains(&args.len()) {
        eprintln!(
            "usage: {} <bytelog> <staging-state-dir> <session-id> <cols> <rows> [dump-text-path] [state-bin-out]",
            args[0]
        );
        std::process::exit(2);
    }
    let dump_text_path = args.get(6).cloned();
    let state_bin_out = args.get(7).cloned();
    let bytelog_path = &args[1];
    let staging_dir = &args[2];
    let session_id: u64 = args[3].parse().expect("session-id must be u64");
    let cols: u16 = args[4].parse().expect("cols must be u16");
    let rows: u16 = args[5].parse().expect("rows must be u16");

    // Refuse to write into the default live state dir by requiring an
    // explicit staging path that exists and is NOT the default.
    let staging = std::path::PathBuf::from(staging_dir);
    std::fs::create_dir_all(staging.join("sessions").join(session_id.to_string()))
        .expect("create staging sessions dir");

    // Route marspot_term::paths at the staging dir, then let
    // Terminal::new pick the File scrollback variant for this id —
    // the exact production write path, so the record format, header,
    // idx layout and wrapped flags all come out right by construction.
    unsafe {
        std::env::set_var("MARSPOT_STATE_DIR", &staging);
        std::env::set_var("MARSPOT_SESSION_ID", session_id.to_string());
    }

    let mut term = marspot_term::terminal::Terminal::new(cols, rows);
    assert!(
        term.grid().file_scrollback_snapshot().is_some(),
        "Terminal did not land on the File scrollback variant — env routing broke"
    );

    // The bytelog rotates: once the live segment passes its size limit
    // it becomes `<path>.1` and a fresh `<path>` takes over.  Replaying
    // only the file named on the command line therefore recovers just
    // the newer half of the history — feed the retired segment first so
    // the stream goes in chronological order, exactly as
    // `ByteLog::segment_paths` orders it.
    let retired = std::path::PathBuf::from(format!("{bytelog_path}.1"));
    let mut segments: Vec<std::path::PathBuf> = Vec::new();
    if retired.exists() {
        segments.push(retired);
    }
    segments.push(std::path::PathBuf::from(bytelog_path));
    let total: u64 = segments
        .iter()
        .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum();
    eprintln!(
        "  replaying {} segment(s), {total} bytes total",
        segments.len()
    );
    let mut buf = vec![0u8; 1 << 20];
    let mut fed: u64 = 0;
    let started = std::time::Instant::now();
    for seg in &segments {
        let mut f = std::fs::File::open(seg).expect("open bytelog segment");
        loop {
            let n = f.read(&mut buf).expect("read bytelog segment");
            if n == 0 {
                break;
            }
            term.feed(&buf[..n]);
            fed += n as u64;
        }
    }
    term.grid().scrollback_flush_for_handoff();

    let lines = term.grid().scrollback_len();
    eprintln!(
        "replayed {fed}/{total} bytes in {:?} → {lines} scrollback lines",
        started.elapsed()
    );
    // Tail sample so the operator can eyeball that the content is the
    // session's real history and not garbage.
    for i in lines.saturating_sub(3)..lines {
        if let Some(cells) = term.grid().scrollback_line(i) {
            let text: String = cells
                .iter()
                .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
                .collect();
            eprintln!("  [{i}] {}", text.trim_end());
        }
    }

    // Optional human-readable dump: full scrollback + the final
    // visible grid.  TUI sessions rewrite in place, so the grid at
    // end-of-bytelog is usually the recoverable payload.
    if let Some(path) = dump_text_path {
        use std::io::Write;
        let mut out =
            std::io::BufWriter::new(std::fs::File::create(&path).expect("create dump-text file"));
        writeln!(out, "# session {session_id} — bytelog replay dump").unwrap();
        writeln!(out, "# scrollback: {lines} lines").unwrap();
        for i in 0..lines {
            if let Some(cells) = term.grid().scrollback_line(i) {
                let text: String = cells
                    .iter()
                    .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
                    .collect();
                writeln!(out, "{}", text.trim_end()).unwrap();
            }
        }
        writeln!(out, "# ---- final visible grid ({cols}x{rows}) ----").unwrap();
        let grid = term.grid();
        for r in 0..grid.rows() {
            let mut line = String::with_capacity(cols as usize);
            for c in 0..grid.cols() {
                let ch = grid.cell(c, r).ch;
                line.push(if ch == '\0' { ' ' } else { ch });
            }
            writeln!(out, "{}", line.trim_end()).unwrap();
        }
        eprintln!("text dump → {path}");
    }

    // Optional v4 snapshot: the replayed terminal's full state —
    // in alt mode this folds the alt ring + final screen into the
    // snapshot so a cold-booting L3 lands them in scrollback.
    if let Some(path) = state_bin_out {
        let body = term.serialize_snapshot();
        std::fs::write(&path, &body).expect("write state-bin-out");
        eprintln!("state.bin ({} bytes) → {path}", body.len());
    }
}
