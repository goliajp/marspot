//! A first frame against a real pty: a stand-in program that keeps
//! printing past the deadline, one that paints and stops, and one that
//! never paints its frame's worth.

use super::*;
use marspot_term::pty::{Pty, PtyConfig, TerminalSize};
use std::time::Instant;

/// The pty's own output as the counter, the wall clock as the clock;
/// the rest of the world is the fake one.
struct PtyEnv {
    io: Arc<dyn PtyIo>,
    master: std::os::fd::RawFd,
    read: Mutex<u64>,
}

impl OpEnv for PtyEnv {
    fn io(&self) -> &Arc<dyn PtyIo> {
        &self.io
    }
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn signal(&self, _: ProcId, _: i32) -> Result<(), SignalError> {
        Ok(())
    }
    fn pid_alive(&self, _: ProcId) -> bool {
        true
    }
    fn find_descendant(&self, _: i32, _: fn(&pidtree::ProcRow) -> bool) -> Option<ProcId> {
        None
    }
    fn find_in_front(&self, _: i32, _: fn(&pidtree::ProcRow) -> bool) -> Option<ProcId> {
        None
    }
    fn front(&self, _: i32) -> front::Front {
        front::Front::Shell
    }
    fn output_len(&self, _: u64) -> u64 {
        let mut total = self.read.lock().unwrap();
        let mut buf = [0u8; 8192];
        loop {
            // SAFETY: a non-blocking descriptor this test owns, into a local buffer
            let n = unsafe { libc::read(self.master, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            *total += n as u64;
        }
        *total
    }
    fn output_tail(&self, _: u64, _: usize) -> Vec<u8> {
        Vec::new()
    }
    fn pane_modes(&self, _: u64) -> Option<(u64, u32)> {
        None
    }
    fn pane_screen(&self, _: u64) -> Option<String> {
        Some(String::new())
    }
    fn pane_unsent(&self, _: u64) -> Option<String> {
        None
    }
}

/// Runs the cycle's first-frame script, with a short deadline, against
/// `script` in a real pty, and returns how it ended and what it typed.
fn first_frame_against(script: &str) -> Option<(OpOutcome, usize)> {
    if std::env::var_os("MARSPOT_PTY_INTEGRATION").is_none() {
        eprintln!("SKIPPED: set MARSPOT_PTY_INTEGRATION=1 (bin/test.sh does)");
        return None;
    }
    let pty = Pty::spawn(PtyConfig {
        program: "/usr/bin/perl".into(),
        // a pause first: what arrives before the step opens is not this frame's
        args: vec!["-e".into(), format!("select(undef, undef, undef, 0.3); {script}")],
        size: TerminalSize { cols: 80, rows: 24, pixel_width: 0, pixel_height: 0 },
        ..PtyConfig::default()
    })
    .expect("spawn the stand-in");
    let master = pty.raw_master();
    // SAFETY: flags of a descriptor this test owns
    unsafe { libc::fcntl(master, libc::F_SETFL, libc::fcntl(master, libc::F_GETFL) | libc::O_NONBLOCK) };
    let state = Arc::new(FakeState::default());
    let env = PtyEnv { io: Arc::new(FakeIo(Arc::clone(&state))), master, read: Mutex::new(0) };
    let held = Job::new();
    held.finish(Ok(Some("half a sentence".into())));
    let op = PtyOp::new("test.cycle")
        .step(
            Step::await_quiet(Duration::from_millis(300))
                .after_bytes(2048)
                .or_composer()
                .or_drawn_late()
                .timeout(Duration::from_millis(1_500))
                .named("first_frame"),
        )
        .step(Step::restore_composer(held));
    let host = Host { ended: Mutex::new(false), badges: Mutex::new(Vec::new()), logs: Mutex::new(Vec::new()) };
    let mut r = OpRunner::new(op, Box::new(env));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !*host.ended.lock().unwrap() && Instant::now() < deadline {
        r.on_tick(&host);
        std::thread::sleep(Duration::from_millis(10));
    }
    r.on_end(&host, EndReason::PluginRequested);
    drop(pty);
    let typed = state.pasted.lock().unwrap().len() + state.sent.lock().unwrap().len();
    Some((r.finished.clone().expect("the run ended"), typed))
}

#[test]
fn a_real_program_still_printing_past_the_deadline_is_drawn_late() {
    let Some((outcome, typed)) = first_frame_against(
        r#"$|=1; while (1) { print "x" x 200, "\r\n"; select(undef, undef, undef, 0.02) }"#,
    ) else {
        return;
    };
    assert_eq!(
        outcome,
        OpOutcome::DrawnLate { step: 0, label: "first_frame", skipped: vec!["restore_composer"] }
    );
    assert_eq!(typed, 0, "nothing typed into a program still drawing");
}

#[test]
fn a_real_program_that_paints_and_stops_is_done() {
    let Some((outcome, typed)) = first_frame_against(r#"$|=1; print "x" x 4000; sleep 30"#) else {
        return;
    };
    assert_eq!(outcome, OpOutcome::Done);
    assert_eq!(typed, 1, "the held line goes back once the frame is drawn");
}

#[test]
fn a_real_program_that_never_paints_a_frame_times_out() {
    let Some((outcome, typed)) = first_frame_against(r#"$|=1; print "x" x 100; sleep 30"#) else {
        return;
    };
    assert_eq!(outcome, OpOutcome::TimedOut { step: 0, label: "first_frame" });
    assert_eq!(typed, 0);
}
