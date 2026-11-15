//! A pane whose history was written before segments, through the
//! session binary that keeps history in segments taking over by its own
//! execv: the old lines stay readable, before the new ones, and the old
//! files stay where they are until the budget passes them.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marspot_term::scrollback::FileScrollback;
use marspot_term::shell_proto::MsgType;

use crate::self_execv::{Sandbox, count_in_log, id, stage, stage_from, wait_for};
use crate::support::{COLS, ROWS, Session};

const OLD: usize = 300;

fn old_line(i: usize) -> String {
    format!("old {i:05}")
}

/// Lines the session's history holds on disk.
fn filed(dir: &Path) -> u64 {
    FileScrollback::on_disk(&dir.join("scrollback.bin"), &dir.join("scrollback.idx")).map_or(0, |(lines, _)| lines)
}

/// Until the history holds `want` lines and has stopped growing.
fn filed_settled(dir: &Path, want: u64, what: &str) {
    let mut last = (0, Instant::now());
    wait_for(60, what, || {
        let n = filed(dir);
        if n != last.0 {
            last = (n, Instant::now());
        }
        (n >= want && last.1.elapsed() > Duration::from_millis(300)).then_some(())
    });
}

/// A shell that prints the numbers in batches of a thousand, the next
/// batch once the file `go.<batch>` exists.
fn batches(state: &Path, n: u32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let mut sh = String::from("#!/bin/sh\nseq 1 1000\n");
    for b in 1..n {
        let go = state.join(format!("go.{b}"));
        sh += &format!("while [ ! -e {} ]; do sleep 0.05; done\nseq {} {}\n", go.display(), b * 1000 + 1, (b + 1) * 1000);
    }
    sh += "exec sleep 600\n";
    let script = state.join("lines.sh");
    std::fs::write(&script, sh).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// Ask the pane's L3 to update to what is staged, and wait until it has
/// replaced itself.  Counted in the log rather than read off the
/// executable's path: a build staged over another one has the same path.
fn update_to(s: &mut Session, state: &Path) {
    let before = count_in_log(state, "L3_EXECV_INVOKE");
    s.send_frame(MsgType::SessionUpdate, Vec::new());
    wait_for(30, "the L3 to replace itself", || {
        assert_eq!(s.child.try_wait().unwrap(), None, "the L3 exited instead of handing over");
        (count_in_log(state, "L3_EXECV_INVOKE") > before).then_some(())
    });
}

fn text(history: &FileScrollback, i: usize) -> String {
    let cells = history.read_line(i).unwrap_or_else(|| panic!("line {i} reads"));
    cells.iter().map(|c| c.ch).collect::<String>().trim_end_matches(['\0', ' ']).to_string()
}

fn bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_marspot-session"))
}

#[test]
fn an_update_keeps_the_history_written_before_segments() {
    let mut sandbox = Sandbox::new();
    let state = sandbox.path().to_path_buf();
    let dir = state.join(format!("sessions/{}", id()));
    let old: Vec<String> = (0..OLD).map(old_line).collect();
    marspot_term::test_env::write_history_before_segments(&dir, &old, b"bytes logged before segments\r\n");
    let script = batches(&state, 2);
    sandbox.env.set("SHELL", &script);
    let mut s = Session::spawn_with_id(bin(), COLS, ROWS, Some(id()));
    let rows = ROWS as u64;
    filed_settled(&dir, OLD as u64 + 1000 + 1 - rows, "the first thousand lines in the history");
    stage(&state, true);
    update_to(&mut s, &state);
    std::fs::write(state.join("go.1"), b"").unwrap();
    filed_settled(&dir, OLD as u64 + 2000 + 1 - rows, "the second thousand lines in the history");
    s.kill();

    for p in ["scrollback.bin", "scrollback.idx", "bytelog"] {
        assert!(dir.join(p).exists(), "{p} was deleted inside the budget");
    }
    let history = FileScrollback::open(dir.join("scrollback.bin"), dir.join("scrollback.idx"), COLS as usize, 4)
        .expect("the history opens");
    for (i, line) in old.iter().enumerate() {
        assert_eq!(&text(&history, i), line, "old line {i}");
    }
    // the new lines follow, in order, across the update
    let numbers: Vec<u64> = (OLD..history.len()).filter_map(|i| text(&history, i).trim().parse().ok()).collect();
    assert_eq!(numbers.first(), Some(&1));
    assert!(numbers.windows(2).all(|w| w[1] == w[0] + 1), "the numbered lines skip or repeat");
    assert!(numbers.len() as u64 >= 2000 + 1 - rows, "{} numbered lines after the old ones", numbers.len());
    let log = marspot_term::bytelog::tail(&dir, u64::MAX).expect("the log reads");
    assert!(log.starts_with(b"bytes logged before segments\r\n"), "the old log is the start of the stream");
}

/// The real thing, both ways: a pane started on a session binary built
/// before segments (`MARSPOT_OLD_SESSION_BIN`), updated to this one,
/// rolled back to the old one and updated again.  Every number printed
/// is in the history once at the end, the old build's lines in its own
/// files and this one's in segments -- in that order, so the lines of
/// the rolled-back stretch come before those of the stretch before it.
#[test]
#[ignore = "needs MARSPOT_OLD_SESSION_BIN, a session binary built before segments"]
fn an_update_from_the_build_before_segments_and_back_loses_no_line() {
    let Some(old_bin) = std::env::var_os("MARSPOT_OLD_SESSION_BIN").map(PathBuf::from) else { return };
    let mut sandbox = Sandbox::new();
    let state = sandbox.path().to_path_buf();
    let dir = state.join(format!("sessions/{}", id()));
    let script = batches(&state, 4);
    sandbox.env.set("SHELL", &script);
    let mut s = Session::spawn_with_id(&old_bin, COLS, ROWS, Some(id()));
    let rows = ROWS as u64;
    filed_settled(&dir, 1000 + 1 - rows, "the old build's first thousand lines");
    assert!(dir.join("scrollback.bin").exists(), "the old build writes the format before segments");
    for (b, to) in [(1u64, bin()), (2, old_bin.as_path()), (3, bin())] {
        stage_from(&state, to, false);
        update_to(&mut s, &state);
        std::fs::write(state.join(format!("go.{b}")), b"").unwrap();
        filed_settled(&dir, (b + 1) * 1000 + 1 - rows, &format!("batch {b} after the update to {}", to.display()));
    }
    s.kill();
    let history = FileScrollback::open(dir.join("scrollback.bin"), dir.join("scrollback.idx"), COLS as usize, 4)
        .expect("the history opens");
    let in_order: Vec<u64> = (0..history.len()).filter_map(|i| text(&history, i).trim().parse().ok()).collect();
    let runs: Vec<(u64, u64)> = in_order.chunk_by(|a, b| *b == a + 1).map(|r| (r[0], r[r.len() - 1])).collect();
    eprintln!("history order: {runs:?}");
    // The old build, rolled back to, cannot read segments: the screen
    // and recent lines it is handed tell it what it is missing, and it
    // files those again in its own files.  So the stretch written by
    // this build before the rollback is there twice afterwards, and
    // nothing is missing.
    let mut numbers = in_order.clone();
    numbers.sort_unstable();
    numbers.dedup();
    let want: Vec<u64> = (1..=4000 + 1 - rows).collect();
    assert_eq!(numbers, want, "every number");
    assert!(runs.iter().all(|(a, b)| a <= b), "{runs:?}");
}
