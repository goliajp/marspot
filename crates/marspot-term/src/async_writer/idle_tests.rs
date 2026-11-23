//! Buffers that give their pages back once output has stopped.

use std::time::{Duration, Instant};

use super::*;
use crate::idle_pages::{KEPT, times_given_back, times_taken_back};

const CAP: usize = 64 * 1024;

fn writer(name: &str) -> (AsyncWriter, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("marspot-aw-idle-{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let f = std::fs::OpenOptions::new().create(true).append(true).open(&path).expect("open");
    (AsyncWriter::new(f, CAP, 3), path)
}

/// `bytes` of a pattern that shows a byte out of place, from offset `at`.
fn pattern(at: usize, bytes: usize) -> Vec<u8> {
    (at..at + bytes).map(|i| (i % 251) as u8).collect()
}

/// A megabyte in 4 KiB writes, as a flood comes.
fn flood(w: &mut AsyncWriter, at: usize) -> usize {
    let mut n = at;
    while n < at + 1024 * 1024 {
        w.write(&pattern(n, 4096));
        n += 4096;
    }
    n
}

/// Wait for the writer to have nothing left to do; how many free
/// buffers it then has, and how many of them have given their pages
/// back.  The time limit only tells never from not yet.
fn settled(w: &AsyncWriter) -> (usize, usize) {
    let give_up = Instant::now() + Duration::from_secs(20);
    loop {
        let (free, given, asleep) = w.queue.free_pages();
        if asleep {
            return (free, given);
        }
        assert!(Instant::now() < give_up, "the writer never went idle: {free} free, {given} given back");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A flood into a pipe nobody reads yet, so that each of `full` buffers
/// is filled and in flight at once whatever the two threads' timing,
/// then read and flushed.  Into an ordinary file a writer that keeps
/// pace has the producer fill the same two in turn.  The buffers are
/// bigger than what the pipe takes with nobody reading.
fn flood_every_buffer(name: &str, full: usize) -> AsyncWriter {
    const BIG: usize = 256 * 1024;
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    let path = std::env::temp_dir().join(format!("marspot-aw-idle-{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path");
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "a pipe to write into");
    let pipe = std::fs::OpenOptions::new().read(true).write(true).open(&path).expect("open the pipe");
    let mut reader = std::fs::File::open(&path).expect("open its other end");
    std::fs::remove_file(&path).expect("unlink the pipe");
    let mut w = AsyncWriter::new(pipe, BIG, full);
    for i in 0..full {
        w.write(&pattern(i * BIG, BIG));
    }
    let read = std::thread::spawn(move || {
        let mut got = vec![0; full * BIG];
        reader.read_exact(&mut got).expect("read the flood");
        got
    });
    w.flush();
    assert!(read.join().expect("the reading thread") == pattern(0, full * BIG));
    w
}

/// After a flood, the free buffers give their pages back once the
/// writer has nothing to do.
#[test]
fn a_flood_s_free_buffers_give_their_pages_back_when_the_writer_is_idle() {
    let given_before = times_given_back();
    let w = flood_every_buffer("free", 3);
    let (free, given) = settled(&w);
    // of the three filled, the flush may have one for the next write
    let held = usize::from(w.buf.given.is_given() || w.buf.high > 0);
    assert_eq!(given + held, 3, "{given} of {free} free buffers given back, {held} held");
    assert!(times_given_back() >= given_before + given);
}

/// A buffer taken to be filled and not written far into -- a flush
/// hands one over empty, a flood may end as it fills the one before --
/// does not take its pages back.
#[test]
fn a_buffer_taken_and_not_filled_keeps_its_pages_given_back() {
    let mut w = flood_every_buffer("unfilled", 3);
    settled(&w);
    let taken = times_taken_back();
    for _ in 0..4 {
        w.flush();
        settled(&w);
    }
    assert_eq!(times_taken_back(), taken, "a flush took pages back");
}

/// Everything written, and in hand an empty buffer that a flood wrote
/// to the brim -- as the one swapped in at the end of a flood is.  The
/// one `flush` swaps in may be one the idle writer has already given
/// back, depending on how soon it went idle.
fn holding_a_buffer_a_flood_filled(w: &mut AsyncWriter) {
    w.flush();
    assert!(w.buf.is_empty());
    w.buf.given.take_back(&mut w.buf.main);
    w.room = CAP;
    w.buf.high = CAP;
}

/// The buffer the producer holds at the end of a burst gives its pages
/// back, all but the start that a small burst needs.
#[test]
fn the_buffer_held_at_the_end_of_a_burst_gives_its_pages_back() {
    let (mut w, path) = writer("held");
    flood(&mut w, 0);
    holding_a_buffer_a_flood_filled(&mut w);
    w.hand_off();
    assert!(w.buf.given.is_given(), "the end of a burst gave nothing back");
    assert!(w.room >= KEPT && w.room < CAP, "room {}", w.room);
    drop(w);
    let _ = std::fs::remove_file(&path);
}

/// A buffer whose pages are given back takes them back at the first
/// write past what it kept, and what is written arrives intact.
#[test]
fn a_write_past_what_was_kept_takes_the_pages_back_and_arrives_whole() {
    let (mut w, path) = writer("past");
    let mut at = flood(&mut w, 0);
    holding_a_buffer_a_flood_filled(&mut w);
    w.hand_off();
    assert!(w.buf.given.is_given());
    let taken_before = times_taken_back();
    let past = w.room + 100;
    w.write(&pattern(at, past));
    at += past;
    assert!(!w.buf.given.is_given(), "written past what it kept with its pages given back");
    assert_eq!(w.room, CAP);
    assert!(times_taken_back() > taken_before);
    at = flood(&mut w, at);
    w.flush();
    let got = std::fs::read(&path).expect("read back");
    assert_eq!(got.len(), at);
    assert!(got == pattern(0, at), "the bytes came back changed");
    drop(w);
    let _ = std::fs::remove_file(&path);
}

/// Small bursts after the pages were given back -- a key's echo, a
/// prompt -- neither take them back nor give any more back.
#[test]
fn small_bursts_take_nothing_back_and_give_nothing_back() {
    let (mut w, path) = writer("small");
    flood(&mut w, 0);
    w.flush();
    w.hand_off();
    settled(&w);
    let (given, taken) = (times_given_back(), times_taken_back());
    for i in 0..50 {
        w.write(&pattern(i, 100));
        w.hand_off();
        // past the writer's linger, so that it goes idle between two
        std::thread::sleep(Duration::from_millis(3));
    }
    assert_eq!(times_taken_back(), taken, "a small burst took pages back");
    assert_eq!(times_given_back(), given, "a small burst gave pages back");
    drop(w);
    let _ = std::fs::remove_file(&path);
}

/// A burst that takes the pages back and writes little of them has them
/// given back when it ends: they count whether or not it wrote them.
#[test]
fn pages_a_smaller_burst_took_back_are_given_back_again() {
    let (mut w, path) = writer("again");
    flood(&mut w, 0);
    holding_a_buffer_a_flood_filled(&mut w);
    w.hand_off();
    assert!(w.buf.given.is_given());
    for _ in 0..3 {
        settled(&w);
        let before = times_given_back();
        w.write(&pattern(0, KEPT + 8 * 1024));
        assert!(!w.buf.given.is_given());
        w.flush();
        settled(&w);
        assert!(times_given_back() > before, "the pages stayed taken");
        // the one written is the last one the writer put back
        let last = w.queue.take_free().expect("a free buffer");
        assert!(last.given.is_given());
        w.swap_in(last);
    }
    drop(w);
    let _ = std::fs::remove_file(&path);
}
