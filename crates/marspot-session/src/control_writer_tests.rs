//! Pokes to a core that is not reading fold into the one still queued,
//! and a core that reads again is woken by the next one.

use super::*;
use std::time::{Duration, Instant};

fn wait_backlog_at_most(w: &ControlWriter, bytes: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.backlog() > bytes {
        assert!(Instant::now() < deadline, "backlog stuck at {}", w.backlog());
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn pokes_to_a_core_that_stopped_reading_stay_one_frame() {
    let (ours, _theirs) = UnixStream::pair().expect("a socket pair");
    let mut w = ControlWriter::new(ours).expect("a writer");
    // fills the socket buffer, then the writer thread blocks on one
    // poke and at most one more waits behind it
    for _ in 0..200_000 {
        w.poke();
    }
    assert!(w.backlog() <= 2 * HEADER_LEN, "backlog {} bytes", w.backlog());
    assert_eq!(w.dropped_bytes(), 0);
}

#[test]
fn a_poke_after_the_core_read_the_last_one_is_sent() {
    let (ours, mut theirs) = UnixStream::pair().expect("a socket pair");
    let mut w = ControlWriter::new(ours).expect("a writer");
    for round in 0..3 {
        w.poke();
        wait_backlog_at_most(&w, 0);
        let f = Frame::read_from(&mut theirs).expect("read").expect("a frame");
        assert_eq!(f.msg_type, MsgType::GridReady, "round {round}");
    }
}

#[test]
fn other_frames_are_not_folded() {
    let (ours, mut theirs) = UnixStream::pair().expect("a socket pair");
    let mut w = ControlWriter::new(ours).expect("a writer");
    w.poke();
    assert!(w.send(Frame::new(MsgType::PaneBell, Vec::new())));
    assert!(w.send(Frame::new(MsgType::PaneBell, Vec::new())));
    drop(w);
    let mut kinds = Vec::new();
    while let Some(f) = Frame::read_from(&mut theirs).expect("read") {
        kinds.push(f.msg_type);
    }
    assert_eq!(kinds, [MsgType::GridReady, MsgType::PaneBell, MsgType::PaneBell]);
}

fn said(output: bool, foreground: bool, fg_pgid: i32) -> PaneAwake {
    PaneAwake { session_id: 7, output, foreground, shell_in_front: false, fg_pgid }
}

#[test]
fn reports_to_a_core_that_stopped_reading_stay_one_frame() {
    let (ours, _theirs) = UnixStream::pair().expect("a socket pair");
    let mut w = ControlWriter::new(ours).expect("a writer");
    for i in 0..200_000 {
        assert!(w.awake(said(i % 2 == 0, true, i)));
    }
    assert!(w.backlog() <= 2 * w.awake_len, "backlog {} bytes", w.backlog());
}

#[test]
fn a_folded_report_keeps_every_change_and_the_latest_group() {
    let (ours, mut theirs) = UnixStream::pair().expect("a socket pair");
    let mut w = ControlWriter::new(ours).expect("a writer");
    // the slot as the writer would find it with one report queued
    *w.awake_queued.lock().unwrap() = Some(said(true, false, 1));
    assert!(w.awake(said(false, true, 2)));
    assert!(w.awake(said(false, false, 3)));
    let queued = w.awake_queued.lock().unwrap().take().expect("still one queued");
    assert_eq!(queued, said(true, true, 3));
    // with nothing queued a report goes out as it is
    assert!(w.awake(said(false, true, 4)));
    drop(w);
    let f = Frame::read_from(&mut theirs).expect("read").expect("a frame");
    assert_eq!(f.msg_type, MsgType::PaneAwake);
    let got = marspot_term::shell_proto::decode_pane_awake(&f.payload).expect("decodes");
    assert_eq!(got, said(false, true, 4));
}
