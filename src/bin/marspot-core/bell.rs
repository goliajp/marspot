//! The bell a pane's program rang.  The session that parsed the BEL
//! has no window, so it says so in a frame and the sound is made here.
//! Nothing else: no flash, no badge, no dock bounce.
//!
//! A window beeps at most once per `QUIET`; bells inside that interval
//! are dropped, not queued.  `yes $'\a'` publishes ~125 bell frames a
//! second, and one beep per frame is a noise nobody can stop.

use super::CoreApp;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Shortest gap between two beeps of one window.
const QUIET: Duration = Duration::from_millis(250);

/// Bells rung so far; a test cannot hear one.
#[cfg(test)]
pub(super) static RUNG: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Every bell goes through here.  `NSBeep` does not require the main
/// thread, but the per-window interval lives on the loop's windows.
fn ring() {
    #[cfg(test)]
    RUNG.fetch_add(1, Ordering::Relaxed);
    #[cfg(not(test))]
    objc2_app_kit::NSBeep();
}

/// Beep unless this window beeped less than `QUIET` before `now`.
fn ring_window(last_bell_at: &mut Option<Instant>, now: Instant) {
    if last_bell_at.is_some_and(|t| now.saturating_duration_since(t) < QUIET) {
        return;
    }
    *last_bell_at = Some(now);
    ring();
}

impl CoreApp {
    /// Session `sid`'s pane rang.  `queued` is its reader's flag, cleared
    /// first so a bell arriving from here on queues a new event.  A pane
    /// in no window (mid-move, already closed) has nowhere to beep.
    pub(super) fn pane_bell(&mut self, sid: u64, queued: &AtomicBool, now: Instant) {
        queued.store(false, Ordering::Release);
        if let Some((wi, _)) = self.find_pane_by_sid(sid) {
            ring_window(&mut self.windows[wi].last_bell_at, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::window_state_tests::{app_with, win};
    use super::super::{CoreEvent, l3_reader_loop};
    use marspot_term::test_env::sandbox_state_dir;
    use super::RUNG;
    use marspot::pane::Pane;
    use marspot::shell_proto::{Frame, MsgType, HEADER_LEN, MAGIC};
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn frame_of_type(t: u32) -> Vec<u8> {
        let mut b = Vec::with_capacity(HEADER_LEN);
        b.extend_from_slice(&MAGIC.to_le_bytes());
        b.extend_from_slice(&t.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b
    }

    /// A session's bell reaches the loop as one `PaneBell` for its
    /// session; a second bell while the first is still queued is folded
    /// into it; a frame this core does not know between them is skipped
    /// without ending the stream: what an older core does with
    /// `PaneBell` itself.
    #[test]
    fn a_bell_frame_queues_one_bell_and_an_unknown_frame_is_skipped() {
        let (mut session, core) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let (selection_tx, _selection_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || l3_reader_loop(core, 7, tx, selection_tx));
        let bell = Frame::new(MsgType::PaneBell, Vec::new());
        bell.write_to(&mut session).unwrap();
        session.write_all(&frame_of_type(250)).unwrap();
        bell.write_to(&mut session).unwrap();
        Frame::new(MsgType::GridReady, Vec::new()).write_to(&mut session).unwrap();
        drop(session);
        reader.join().unwrap();
        let events: Vec<CoreEvent> = rx.try_iter().collect();
        assert!(matches!(&events[0], CoreEvent::PaneBell(7, q) if q.load(Ordering::Relaxed)));
        assert!(matches!(events[1], CoreEvent::L3Ready), "the frame after the bells still arrives");
        assert!(matches!(events[2], CoreEvent::L3ControlEof(7)));
        assert_eq!(events.len(), 3);
    }

    /// 1000 bells over one second in one window beep 4 times (at 0, 250,
    /// 500 and 750 ms); another window keeps its own interval.
    #[test]
    fn a_thousand_bells_in_a_second_beep_at_most_four_times_per_window() {
        let sandbox = sandbox_state_dir();
        let mut app = app_with(
            &sandbox,
            vec![win(1, vec![Pane::new_vacant(7, 80, 24)]), win(2, vec![Pane::new_vacant(8, 80, 24)])],
        );
        let queued = AtomicBool::new(true);
        let t0 = Instant::now();
        let before = RUNG.load(Ordering::Relaxed);
        for i in 0..1000 {
            app.pane_bell(7, &queued, t0 + Duration::from_millis(i));
        }
        assert!(!queued.load(Ordering::Relaxed), "handling a bell clears its reader's flag");
        assert_eq!(RUNG.load(Ordering::Relaxed) - before, 4);
        app.pane_bell(8, &queued, t0 + Duration::from_millis(999));
        assert_eq!(RUNG.load(Ordering::Relaxed) - before, 5, "the other window was quiet");
        app.pane_bell(99, &queued, t0 + Duration::from_secs(5));
        assert_eq!(RUNG.load(Ordering::Relaxed) - before, 5, "a pane in no window has nowhere to beep");
    }

    /// End to end: 1000 bell frames written as fast as the socket takes
    /// them, the loop's side handled as events arrive, beep at most 4
    /// times (they all land well inside one second).
    #[test]
    fn a_thousand_bell_frames_through_the_reader_beep_at_most_four_times() {
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![win(1, vec![Pane::new_vacant(7, 80, 24)])]);
        let (mut session, core) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let (selection_tx, _selection_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || l3_reader_loop(core, 7, tx, selection_tx));
        let writer = std::thread::spawn(move || {
            let bell = Frame::new(MsgType::PaneBell, Vec::new());
            for _ in 0..1000 {
                bell.write_to(&mut session).unwrap();
            }
        });
        let t0 = Instant::now();
        let before = RUNG.load(Ordering::Relaxed);
        let mut handled = 0;
        loop {
            match rx.recv().unwrap() {
                CoreEvent::PaneBell(sid, queued) => {
                    handled += 1;
                    app.pane_bell(sid, &queued, Instant::now());
                }
                CoreEvent::L3ControlEof(7) => break,
                _ => {}
            }
        }
        writer.join().unwrap();
        reader.join().unwrap();
        assert!(t0.elapsed() < Duration::from_secs(1), "the bells did not all land inside one second");
        assert!(handled >= 1);
        assert!(RUNG.load(Ordering::Relaxed) - before <= 4);
    }
}
