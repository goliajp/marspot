//! The frames a flood's turns put out while a report is owed: a watch
//! met publishes the frame kept back once, and after that the flood's
//! spacing holds, however long the report takes to leave.

use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use marspot_term::grid_shm::{GridShmReader, GridShmWriter};
use marspot_term::loop_watch::LoopWatch;
use marspot_term::shell_proto::{Frame, MsgType, PaneWatch, WatchBody, WatchKind};

use crate::SessionImpl;
use crate::control_writer::ControlWriter;
use crate::frames::{Frames, Shown};
use crate::local_session::LocalSession;
use crate::pane_ctl::{PaneCtl, Request};
use crate::publish_gate::FloodGate;

const TURNS: usize = 400;

/// Flood turns with a watch met in the first and its report refused, if
/// L2 is there to send it to: the frames that went out, and how many
/// the spacing allows in the time they took.
fn flood(mut poke: Option<&mut ControlWriter>) -> (u64, u64) {
    let (local, tx) = LocalSession::parsing_only(40, 10);
    let mut session = SessionImpl::Local(local);
    let (wake, _woken) = std::sync::mpsc::channel();
    let mut ctl = PaneCtl::new(&session, Default::default(), wake);
    ctl.refuse_reports(u32::MAX);
    let mut frames = Frames::new(GridShmWriter::create(40, 10).expect("a region"));
    let watch = WatchBody { watch_id: 1, kind: WatchKind::Change, since_pos: 0, quiet_ms: 0, text: String::new() };
    ctl.request(Request::Watch(PaneWatch { req_id: 1, sid: 1, watch }), &mut session, None);
    let chunks: Vec<Vec<u8>> = (0..TURNS).map(burst).collect();
    let reader = GridShmReader::from_fd(frames.shm().fd()).expect("map the region");
    let mut loop_watch = LoopWatch::new(Duration::from_secs(60));
    let before = reader.seq();
    let started = Instant::now();
    for chunk in chunks {
        tx.send(chunk.into()).expect("queue a chunk");
        let shown = Shown { predicted: false, resized: false, modes_changed: false, scroll: None };
        frames.turn(&mut session, poke.as_deref_mut(), &mut loop_watch, shown);
        ctl.after_turn(&mut session, Some(&mut frames), false, poke.as_deref_mut());
    }
    let allowed = (started.elapsed().as_nanos() / FloodGate::FRAME.as_nanos()) as u64;
    ((reader.seq() - before) / 2, allowed)
}

/// A turn's worth of output that no other turn has, so that every frame
/// differs.
fn burst(turn: usize) -> Vec<u8> {
    let mut out = vec![b'x'; FloodGate::BULK_BYTES];
    out.extend_from_slice(format!("\r\nturn {turn}\r\n").as_bytes());
    out
}

/// One frame goes out with the first turn, one more at most as the watch
/// is met, and the rest are spaced.
fn assert_spaced((frames, allowed): (u64, u64)) {
    assert!((1..=allowed + 2).contains(&frames), "{frames} frames went out, {allowed} were due");
}

#[test]
fn watch_met_while_l2_is_gone_leaves_a_floods_frames_spaced() {
    assert_spaced(flood(None));
}

#[test]
fn watch_report_refused_leaves_a_floods_frames_spaced() {
    let (ours, mut theirs) = UnixStream::pair().expect("a socket pair");
    let mut poke = ControlWriter::new(ours).expect("a writer");
    assert_spaced(flood(Some(&mut poke)));
    drop(poke);
    while let Some(f) = Frame::read_from(&mut theirs).expect("a frame") {
        assert_ne!(f.msg_type, MsgType::PaneWatchFired, "every report was refused");
    }
}
