//! A mark filed against a line is read back on that line, or not at all.
//!
//! The scrollback's line numbers are positions, not identities: they
//! are reused. Five paths move them -- a reflow re-cuts every line, a
//! rotation hands the file over and starts a new one, a crash leaves a
//! truncated tail, a corrupt pair is quarantined and replaced, and a
//! rotated-past generation is gone. After any of them, a record filed
//! under the old numbering must be unreachable rather than wrong,
//! because a mark on the wrong line is exactly what a jump lands on.
//!
//! Path 4, rotation, is in `line_identity_rotation.rs`: it has to set
//! the hot-file cap, and an env var is process-wide, so a test that
//! sets one cannot share a binary with tests that depend on the
//! default.  It poisoned all five of these on the first run.
//!
//! What makes that hold is one rule: the sidecar states the epoch of
//! the `.bin` it was built against, and a mismatch discards it whole.
//! `scrollback::sidecar`'s own tests include the reverse case -- with
//! the epoch comparison removed, the stale record comes back -- so the
//! rule is load-bearing rather than decorative.

use marspot_term::grid::PromptMark;
use marspot_term::scrollback::Scrollback;

struct Dir(std::path::PathBuf);
impl Dir {
    fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "marspot-lineid-{}-{}-{}",
            label,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn bin(&self) -> std::path::PathBuf {
        self.0.join("scrollback.bin")
    }
    fn idx(&self) -> std::path::PathBuf {
        self.0.join("scrollback.idx")
    }
    fn open(&self, cols: usize) -> Scrollback {
        Scrollback::file(self.bin(), self.idx(), cols, 8).unwrap()
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn row(c: u8, cols: usize) -> Vec<marspot_term::grid::Cell> {
    (0..cols)
        .map(|_| marspot_term::grid::Cell {
            ch: c as char,
            ..Default::default()
        })
        .collect()
}

/// Push `n` lines and mark the one at `mark_at` (0-based among them).
fn push_marked(sb: &mut Scrollback, n: usize, cols: usize, mark_at: usize, mark: PromptMark) {
    for i in 0..n {
        sb.push_line_with_wrapped(&row(b'a' + (i % 26) as u8, cols), false);
        if i == mark_at {
            sb.mark_last_line(mark);
        }
    }
}

/// The reason the sidecar exists at all: the mark outlives the process
/// that filed it. An in-process mirror cannot do this, and an L3 that
/// re-execs itself gets exactly this situation -- a fresh mirror
/// against a history thousands of lines long.
#[test]
fn a_mark_survives_closing_and_reopening_the_files() {
    let d = Dir::new("reopen");
    {
        let mut sb = d.open(8);
        push_marked(&mut sb, 20, 8, 4, PromptMark::PromptStart);
        assert_eq!(sb.prompt_at(4), PromptMark::PromptStart);
    }
    let sb = d.open(8);
    assert_eq!(
        sb.prompt_at(4),
        PromptMark::PromptStart,
        "a different process reading the same files sees the mark"
    );
    assert_eq!(sb.prompt_at(5), PromptMark::None, "and only on its own line");
}

#[test]
fn an_exit_status_survives_the_round_trip_including_no_status() {
    let d = Dir::new("status");
    {
        let mut sb = d.open(8);
        push_marked(&mut sb, 6, 8, 1, PromptMark::CommandEnd(Some(0)));
        sb.push_line_with_wrapped(&row(b'x', 8), false);
        sb.mark_last_line(PromptMark::CommandEnd(None));
        sb.push_line_with_wrapped(&row(b'y', 8), false);
        sb.mark_last_line(PromptMark::CommandEnd(Some(130)));
    }
    let sb = d.open(8);
    assert_eq!(sb.prompt_at(1), PromptMark::CommandEnd(Some(0)));
    assert_eq!(
        sb.prompt_at(6),
        PromptMark::CommandEnd(None),
        "a shell that did not say is not a shell that said zero"
    );
    assert_eq!(sb.prompt_at(7), PromptMark::CommandEnd(Some(130)));
}

/// Path 1 -- reflow. `restart` truncates the file and the caller pushes
/// re-wrapped lines back, so line 4 means a different line afterwards.
#[test]
fn a_reflow_refuses_the_marks_of_the_width_before_it() {
    let d = Dir::new("reflow");
    let mut sb = d.open(8);
    push_marked(&mut sb, 20, 8, 4, PromptMark::PromptStart);
    let before = sb.epoch();
    assert_eq!(sb.prompt_at(4), PromptMark::PromptStart);

    sb.restart(5);
    assert_ne!(sb.epoch(), before, "reflow is a new generation");
    for i in 0..20 {
        sb.push_line_with_wrapped(&row(b'a' + (i % 26) as u8, 5), false);
    }
    assert_eq!(
        sb.prompt_at(4),
        PromptMark::None,
        "line 4 at five columns is not line 4 at eight"
    );
}

/// Path 2 -- quarantine. A pair of the format before segments whose
/// header does not validate is renamed aside, and the history that
/// replaces it inherits nothing from it.
#[test]
fn a_quarantined_pair_leaves_its_marks_behind() {
    let d = Dir::new("quarantine");
    let mut bin = vec![0u8; 32];
    bin.extend_from_slice(&[9; 64]);
    std::fs::write(d.bin(), &bin).unwrap();
    std::fs::write(d.idx(), [32u64.to_le_bytes(), 48u64.to_le_bytes()].concat()).unwrap();
    std::fs::write(d.0.join("scrollback.bin.marks"), [7u8; 64]).unwrap();
    let mut sb = d.open(8);
    assert!(
        std::fs::read_dir(&d.0)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".corrupt-")),
        "the fixture has to actually trigger the quarantine"
    );
    push_marked(&mut sb, 6, 8, 5, PromptMark::PromptStart);
    let marked: Vec<usize> = (0..sb.len()).filter(|i| sb.prompt_at(*i) != PromptMark::None).collect();
    assert_eq!(marked, vec![5], "the fresh history inherits nothing from the quarantined pair");
}

/// Path 3 -- a crash-truncated tail.  The lines that survived kept their
/// numbers, so their marks are still theirs.
#[test]
fn a_truncated_tail_keeps_the_marks_of_the_lines_that_survived() {
    let d = Dir::new("truncate");
    {
        let mut sb = d.open(8);
        push_marked(&mut sb, 30, 8, 2, PromptMark::PromptStart);
        let mut sb = sb;
        sb.push_line_with_wrapped(&row(b'z', 8), false);
        sb.mark_last_line(PromptMark::CommandEnd(Some(7)));
    }
    // The last line's index entry never written, which is what a crash
    // between the records and the index leaves: that line is not one.
    {
        use std::os::unix::fs::FileExt;
        let (lines, _) = marspot_term::scrollback::FileScrollback::on_disk(&d.bin(), &d.idx()).unwrap();
        let seg = std::fs::OpenOptions::new().read(true).write(true).open(d.0.join("scrollback.seg.0")).unwrap();
        let mut data = [0u8; 8];
        seg.read_exact_at(&mut data, 16).unwrap();
        let index_at = 64 + u64::from_le_bytes(data);
        seg.write_all_at(&[0u8; 8], index_at + (lines - 1) * 8).unwrap();
    }
    let sb = d.open(8);
    assert_eq!(
        sb.prompt_at(2),
        PromptMark::PromptStart,
        "line 2 is still line 2; a truncated tail did not renumber it"
    );
}


/// Not a path that moves numbers, but the one that proves the marks are
/// per-line rather than per-file: several marks, each on its own line.
/// A mirror that pushed two entries per line passed "exactly one mark
/// exists" and failed this.
#[test]
fn each_mark_sits_on_the_line_it_was_filed_against() {
    let d = Dir::new("align");
    let want = [
        (0usize, PromptMark::PromptStart),
        (3, PromptMark::InputStart),
        (4, PromptMark::OutputStart),
        (9, PromptMark::CommandEnd(Some(1))),
    ];
    {
        let mut sb = d.open(8);
        for i in 0..12 {
            sb.push_line_with_wrapped(&row(b'a' + i as u8, 8), false);
            if let Some((_, m)) = want.iter().find(|(at, _)| *at == i) {
                sb.mark_last_line(*m);
            }
        }
    }
    let sb = d.open(8);
    for i in 0..12 {
        let expect = want
            .iter()
            .find(|(at, _)| *at == i)
            .map(|(_, m)| *m)
            .unwrap_or(PromptMark::None);
        assert_eq!(sb.prompt_at(i), expect, "line {i}");
    }
}

/// Path 5 -- clearing the history. `clear` truncates the file back to
/// its header and the next lines pushed take indices from zero again,
/// so this is the same situation reflow is in: a local line number
/// comes back meaning a different line.
#[test]
fn clearing_the_history_refuses_the_marks_of_what_was_cleared() {
    let d = Dir::new("clear");
    let mut sb = d.open(8);
    push_marked(&mut sb, 20, 8, 4, PromptMark::PromptStart);
    assert_eq!(sb.prompt_at(4), PromptMark::PromptStart);

    sb.clear();
    for i in 0..20 {
        sb.push_line_with_wrapped(&row(b'A' + (i % 26) as u8, 8), false);
    }
    assert_eq!(
        sb.prompt_at(4),
        PromptMark::None,
        "line 4 after a clear is not the line 4 that was marked"
    );
}
