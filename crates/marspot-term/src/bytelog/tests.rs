use super::*;
use std::os::unix::fs::FileExt;

/// Segments of these tests: 4 KiB growing to 16 KiB.
const FIRST: u64 = 4096;
const FULL: u64 = 16384;

fn small(dir: &Path) -> ByteLog {
    ByteLog::open_in(dir, FIRST, FULL).expect("open the log")
}

/// The stream as a reader of the files sees it.
fn read_all(dir: &Path) -> Vec<u8> {
    tail(dir, u64::MAX).expect("the log reads back")
}

/// Bytes `0..n` of a stream whose every byte says where it is.
fn counted(from: usize, n: usize) -> Vec<u8> {
    (from..from + n).map(|i| (i % 251) as u8).collect()
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> =
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

/// What has been deleted behind has been, once the thread that does it
/// gets round to it.
fn settle_deletes(dir: &Path, want: usize) -> Vec<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let n = names(dir);
        if n.len() <= want || std::time::Instant::now() > deadline {
            return n;
        }
        std::thread::yield_now();
    }
}

/// However much is logged, what is kept is the newest bytes, in order
/// and without a gap, at most the budget of them and at least one full
/// segment's worth, in two files of the full size.
#[test]
fn the_log_keeps_the_newest_bytes_within_its_budget() {
    let sandbox = crate::test_env::sandbox_state_dir();
    let dir = sandbox.path().join("log");
    let mut log = small(&dir);
    let total = 20 * FULL as usize + 123;
    let all = counted(0, total);
    for chunk in all.chunks(1000) {
        log.append(chunk).unwrap();
    }
    log.flush();
    let kept = read_all(&dir);
    assert!(kept.len() as u64 >= FULL && kept.len() as u64 <= BYTELOG_CAP_BYTES.min(2 * FULL), "{} kept", kept.len());
    assert_eq!(kept, all[total - kept.len()..], "the newest bytes, in order");
    assert_eq!(stream_len(&dir).unwrap(), total as u64, "the length counts every byte ever logged");
    let left = settle_deletes(&dir, 2);
    assert_eq!(left, ["bytelog.seg.2", "bytelog.seg.3"], "two full segments and nothing else");
    for n in &left {
        assert_eq!(std::fs::metadata(dir.join(n)).unwrap().len(), HEADER_BYTES + FULL);
    }
}

/// An append that changes segments returns while the writer thread is
/// stuck, and the stream is whole across the change once it gets on.
#[test]
fn a_changing_append_does_not_wait_for_the_writer() {
    let sandbox = crate::test_env::sandbox_state_dir();
    let dir = sandbox.path().join("log");
    let mut log = small(&dir);
    log.append(&counted(0, 3000)).unwrap();
    // the next segment is asked for at half: let it be made
    log.flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while log.pending.as_ref().is_some_and(|p| !p.is_made()) {
        assert!(std::time::Instant::now() < deadline, "the next segment was never made");
        std::thread::yield_now();
    }
    let (release, stuck) = std::sync::mpsc::channel::<()>();
    log.file.cut(move |_| {
        let _ = stuck.recv();
    });
    log.hand_off();
    let (done_tx, done) = std::sync::mpsc::channel();
    let appender = std::thread::spawn(move || {
        log.append(&counted(3000, 3000)).unwrap();
        let _ = done_tx.send(());
        log
    });
    let returned = done.recv_timeout(std::time::Duration::from_secs(20)).is_ok();
    release.send(()).unwrap();
    let mut log = appender.join().unwrap();
    assert!(returned, "the append that changes segments waited for the writer thread");
    assert_eq!(log.segs.len(), 2, "the fixture has to change segments");
    log.flush();
    assert_eq!(read_all(&dir), counted(0, 6000));
}

/// A segment used again is read from its new start to its written end:
/// what its earlier use left past that is never part of the stream.
#[test]
fn bytes_of_an_earlier_use_are_not_read() {
    let sandbox = crate::test_env::sandbox_state_dir();
    let dir = sandbox.path().join("log");
    let mut log = small(&dir);
    // up to two full segments, then a little into the older one again
    let mut at = 0;
    while log.segs.iter().filter(|s| s.hdr.data == FULL).count() < 2 {
        log.append(&counted(at, 500)).unwrap();
        at += 500;
    }
    let reused_slot = log.segs[0].slot;
    while log.segs.last().unwrap().slot != reused_slot {
        log.append(&counted(at, 500)).unwrap();
        at += 500;
    }
    log.append(&counted(at, 10)).unwrap();
    at += 10;
    log.flush();
    drop(log);
    let kept = read_all(&dir);
    assert_eq!(kept, counted(at - kept.len(), kept.len()), "the stream ends at the bytes last written");
    // the older use's bytes are still in the file, past the written end
    let file = std::fs::read(dir.join(format!("bytelog.seg.{reused_slot}"))).unwrap();
    assert!(file.len() as u64 > HEADER_BYTES + 1000 && file[HEADER_BYTES as usize + 500..].iter().any(|&b| b != 0));
    // and a reopened log goes on after the last byte, not after them
    let mut log = small(&dir);
    log.append(b"after").unwrap();
    log.flush();
    assert!(read_all(&dir).ends_with(&[&counted(at - 10, 10)[..], b"after"].concat()));
}

/// Bytes written past where the header says the writing got -- a kill
/// between the write and the statement -- are not read, and the next
/// process writes over them.
#[test]
fn bytes_past_the_written_end_are_not_read() {
    let sandbox = crate::test_env::sandbox_state_dir();
    let dir = sandbox.path().join("log");
    let mut log = small(&dir);
    log.append(b"kept").unwrap();
    log.flush();
    let seg = log.segs.last().unwrap();
    let (file, end) = (Arc::clone(&seg.file), written_end(seg));
    drop(log);
    file.write_all_at(b"never stated", end).unwrap();
    assert_eq!(read_all(&dir), b"kept");
    assert_eq!(stream_len(&dir).unwrap(), 4);
    let mut log = small(&dir);
    log.append(b"+").unwrap();
    log.flush();
    assert_eq!(read_all(&dir), b"kept+");
}

/// The files of the rotating pair are the oldest bytes, the segments
/// after them; replayed in that order they rebuild the pane, and the
/// length goes on from theirs.
#[test]
fn old_segments_replay_before_the_new_ones() {
    let mut sandbox = crate::test_env::sandbox_state_dir();
    sandbox.env.remove("MARSPOT_SESSION_ID");
    let dir = sandbox.path().join("log");
    std::fs::create_dir_all(&dir).unwrap();
    let all = program_output();
    let (older, rest) = all.split_at(all.len() / 3);
    let (old, newer) = rest.split_at(rest.len() / 2);
    std::fs::write(dir.join("bytelog.1"), older).unwrap();
    std::fs::write(dir.join("bytelog"), old).unwrap();
    let mut log = small(&dir);
    log.append(newer).unwrap();
    log.flush();
    assert_eq!(stream_len(&dir).unwrap(), all.len() as u64);

    let mut live = crate::terminal::Terminal::new(40, 10);
    live.feed(&all);
    let mut replayed = crate::terminal::Terminal::new(40, 10);
    let fed = replay(&pieces(&dir).unwrap(), &mut replayed).unwrap();
    assert_eq!(fed, all.len() as u64);
    assert_same_terminal(&live, &replayed);
    assert_eq!(read_all(&dir), all);
}

/// The old files go when the budget passes them, not before.
#[test]
fn old_files_are_deleted_by_the_budget() {
    let sandbox = crate::test_env::sandbox_state_dir();
    let dir = sandbox.path().join("log");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("bytelog.1"), b"older").unwrap();
    std::fs::write(dir.join("bytelog"), b"old").unwrap();
    std::fs::write(dir.join("bytelog.1.old"), b"left behind").unwrap();
    let mut log = small(&dir);
    assert!(!dir.join("bytelog.1.old").exists(), "what a dead process left was not deleted on open");
    log.append(&counted(0, FULL as usize)).unwrap();
    log.flush();
    assert!(dir.join("bytelog.1").exists() && dir.join("bytelog").exists(), "inside the budget they stay");
    assert!(read_all(&dir).starts_with(b"olderold"));
    log.append(&counted(0, 3 * FULL as usize)).unwrap();
    log.flush();
    let left = settle_deletes(&dir, 2);
    assert!(!left.iter().any(|n| n == "bytelog" || n == "bytelog.1"), "{left:?}");
}

/// What a pane printed: enough to scroll several screens off the top,
/// with colour, wide characters, a carriage-return overwrite and a
/// cursor move, so a replay that only gets plain ASCII right still
/// fails.
fn program_output() -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..120 {
        out.extend_from_slice(format!("\x1b[3{}mline {i:03}\x1b[0m 漢字 ✓ {}\r\n", i % 8, "x".repeat(i % 37)).as_bytes());
        if i % 17 == 0 {
            out.extend_from_slice(b"overwritten\rOVER\r\n");
        }
    }
    out.extend_from_slice(b"\x1b[3;5Hmid-screen\x1b[10;1Hprompt$ ");
    out
}

fn assert_same_terminal(a: &crate::terminal::Terminal, b: &crate::terminal::Terminal) {
    let (ga, gb) = (a.grid(), b.grid());
    assert_eq!(ga.scrollback_len(), gb.scrollback_len(), "scrollback length");
    assert!(ga.scrollback_len() > 50, "the output scrolled well past one screen");
    for i in 0..ga.scrollback_len() {
        assert_eq!(ga.scrollback_line(i), gb.scrollback_line(i), "scrollback line {i}");
    }
    for r in 0..ga.rows() {
        for c in 0..ga.cols() {
            assert_eq!(ga.cell(c, r), gb.cell(c, r), "cell ({c}, {r})");
        }
    }
    assert_eq!(ga.cursor(), gb.cursor(), "cursor");
}

/// The bytes a pane parsed, logged as it parsed them, rebuild the same
/// grid and scrollback in a fresh terminal.  The live side is fed in
/// small odd-sized chunks, so escape sequences and UTF-8 characters
/// straddle the chunk boundaries the way PTY reads cut them, and across
/// segment changes.
#[test]
fn a_replayed_log_rebuilds_the_same_screen_and_scrollback() {
    let mut sandbox = crate::test_env::sandbox_state_dir();
    sandbox.env.remove("MARSPOT_SESSION_ID");
    let id = 21;
    let mut log = ByteLog::open(id).unwrap();
    let mut live = crate::terminal::Terminal::new(40, 10);
    for chunk in program_output().chunks(7) {
        live.feed(chunk);
        log.append(chunk).unwrap();
    }
    log.flush();
    let dir = sandbox.path().join("sessions").join(id.to_string());
    let mut replayed = crate::terminal::Terminal::new(40, 10);
    let fed = replay(&pieces(&dir).unwrap(), &mut replayed).unwrap();
    assert_eq!(fed, program_output().len() as u64);
    assert_same_terminal(&live, &replayed);
}

/// The length a reader sees moves with every byte handed off and stays
/// put while nothing is, and the tail is the last bytes across
/// segments.
#[test]
fn the_length_and_the_tail_follow_the_writing() {
    let sandbox = crate::test_env::sandbox_state_dir();
    let dir = sandbox.path().join("log");
    let mut log = small(&dir);
    assert_eq!(stream_len(&dir).unwrap(), 0);
    log.append(b"hello ").unwrap();
    log.hand_off();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while stream_len(&dir).unwrap() != 6 {
        assert!(std::time::Instant::now() < deadline, "a handed-off burst never reached the file");
        std::thread::yield_now();
    }
    assert_eq!(stream_len(&dir).unwrap(), 6, "nothing more said, nothing more counted");
    let all = counted(0, 3 * FIRST as usize);
    log.append(&all).unwrap();
    log.flush();
    assert!(log.segs.len() >= 2, "the fixture spans segments");
    assert_eq!(stream_len(&dir).unwrap(), 6 + all.len() as u64);
    assert_eq!(tail(&dir, 5000).unwrap(), all[all.len() - 5000..]);
    assert_eq!(stream_len(&sandbox.path().join("none")).unwrap_err().kind(), io::ErrorKind::NotFound);
}
