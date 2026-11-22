use super::*;
use marspot::shell_proto::{
    PaneWatchFired, WatchReason, decode_pane_watch_fired, encode_pane_watch_fired,
};
use std::io::Write;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// A sink that takes each write only once the test lets it, and keeps
/// what it took.
struct Gated {
    gate: Receiver<()>,
    got: Arc<Mutex<Vec<u8>>>,
}

impl Write for Gated {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.gate
            .recv()
            .map_err(|_| std::io::ErrorKind::BrokenPipe)?;
        self.got.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn fired(sid: u64, watch_id: u32, pos: u64) -> Frame {
    let m = PaneWatchFired {
        sid,
        watch_id,
        reason: WatchReason::Met,
        pos,
        status: None,
        line: None,
    };
    Frame::new(MsgType::PaneWatchFired, encode_pane_watch_fired(&m))
}

fn wait_drained(w: &FrameWriter, open: &Sender<()>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while w.backlog() > 0 {
        assert!(Instant::now() < deadline, "the writer never drained");
        let _ = open.send(());
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Frames read back from the bytes the sink took.
fn frames(bytes: &[u8]) -> Vec<Frame> {
    let mut r = bytes;
    let mut out = Vec::new();
    while let Some(f) = Frame::read_from(&mut r).expect("whole frames") {
        out.push(f);
    }
    out
}

#[test]
fn watch_report_refused_by_a_full_l1_queue_goes_on_the_next_turn() {
    let (open, gate) = mpsc::channel();
    let got = Arc::new(Mutex::new(Vec::new()));
    let first = fired(1, 1, 10);
    let size = first.payload.len() + 12;
    // the first frame sits in the writer, so the second does not fit
    let w = FrameWriter::new(
        "test-l1",
        Gated {
            gate,
            got: got.clone(),
        },
        size + size / 2,
    );
    let mut reports = Reports::default();
    reports.push(first);
    reports.push(fired(1, 2, 11));
    assert_eq!(reports.flush(&w), 1, "the queue took one");
    assert_eq!(reports.len(), 1, "the refused one is held");
    assert_eq!(
        reports.retry_after(),
        Some(RETRY),
        "and the loop wakes for it"
    );

    wait_drained(&w, &open);
    assert_eq!(reports.flush(&w), 1, "taken on the next turn");
    assert!(reports.is_empty());
    assert_eq!(reports.retry_after(), None, "nothing held, no wake");
    wait_drained(&w, &open);
    let took: Vec<_> = frames(&got.lock().unwrap())
        .iter()
        .map(|f| decode_pane_watch_fired(&f.payload).unwrap().watch_id)
        .collect();
    assert_eq!(took, [1, 2], "both, in order");
}

#[test]
fn watch_report_for_the_same_watch_replaces_the_held_one() {
    let mut reports = Reports::default();
    reports.push(ended(3, 9, 1, WatchReason::Restarting));
    reports.push(fired(4, 9, 1));
    reports.push(ended(3, 9, 2, WatchReason::SessionEnded));
    assert_eq!(reports.len(), 2);
    assert_eq!(
        held(&reports),
        [(3, 2), (4, 1)],
        "the newer report, in the older one's place"
    );
}

#[test]
fn watch_report_met_is_kept_over_a_later_one() {
    let mut reports = Reports::default();
    reports.push(fired(3, 9, 1));
    reports.push(ended(3, 9, 2, WatchReason::Restarting));
    reports.push(ended(3, 9, 3, WatchReason::SessionEnded));
    assert_eq!(held(&reports), [(3, 1)], "the met report stays");
}

fn ended(sid: u64, watch_id: u32, pos: u64, reason: WatchReason) -> Frame {
    let m = PaneWatchFired {
        sid,
        watch_id,
        reason,
        pos,
        status: None,
        line: None,
    };
    Frame::new(MsgType::PaneWatchFired, encode_pane_watch_fired(&m))
}

fn held(reports: &Reports) -> Vec<(u64, u64)> {
    reports
        .queue
        .iter()
        .map(|f| decode_pane_watch_fired(&f.payload).unwrap())
        .map(|m| (m.sid, m.pos))
        .collect()
}

#[test]
fn watch_reports_survive_a_flood_of_typing_answers() {
    let mut reports = Reports::default();
    reports.push(fired(1, 1, 1));
    for i in 0..(MAX_ANSWERS as u32 + 10) {
        reports.push(Frame::new(MsgType::PaneTyped, i.to_le_bytes().to_vec()));
    }
    assert_eq!(
        reports.len(),
        MAX_ANSWERS + 1,
        "answers capped, the report kept"
    );
    assert_eq!(reports.queue[0].msg_type, MsgType::PaneWatchFired);
    assert_eq!(
        reports.queue[1].payload,
        10u32.to_le_bytes(),
        "the oldest answers went"
    );

    for i in 0..(MAX_FIRED as u32 + 3) {
        reports.push(fired(2, i, 0));
    }
    let n_fired = reports
        .queue
        .iter()
        .filter(|f| f.msg_type == MsgType::PaneWatchFired)
        .count();
    assert_eq!(n_fired, MAX_FIRED, "distinct ended watches are bounded too");
}
