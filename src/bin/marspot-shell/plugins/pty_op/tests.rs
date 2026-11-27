// CARVE-OUT: test module for pty_op, scenarios against a fake process table and clock
use super::*;
use std::sync::Mutex;

/// A world the test owns: a clock it advances by hand, a process
/// table it edits, and a record of everything the op did.
struct FakeEnv {
    io: Arc<dyn PtyIo>,
    state: Arc<FakeState>,
}

/// One pane runs one script at a time; the rest wait their turn.
///
/// This is the invariant the service exists for.  Two scripts
/// typing into the same PTY interleave their keystrokes, and what
/// arrives is neither command — a failure that only shows up once
/// something submits ops it does not own, which is exactly what
/// session-to-session delivery is.
#[test]
fn one_run_at_a_time_per_pane_and_the_rest_queue() {
    let state = Arc::new(FakeState::default());
    let mut ops = PtyOps::new(Arc::new(FakeIo(Arc::clone(&state))) as Arc<dyn PtyIo>);
    let host = FakePluginHost::default();

    let first = ops.submit(7, PtyOp::new("test.a").step(Step::settle(Duration::from_secs(1))));
    let second = ops.submit(7, PtyOp::new("test.b").step(Step::settle(Duration::from_secs(1))));
    let other_pane = ops.submit(9, PtyOp::new("test.c").step(Step::settle(Duration::from_secs(1))));
    assert!(first.is_some() && second.is_some() && other_pane.is_some());

    ops.pump(&host);
    assert_eq!(
        *host.begun.lock().unwrap(),
        vec![(7, "test.a".to_string()), (9, "test.c".to_string())],
        "one per pane starts; a different pane is not blocked by it"
    );
    assert_eq!(ops.running(7), Some("test.a"));

    ops.pump(&host);
    assert_eq!(
        host.begun.lock().unwrap().len(),
        2,
        "the queued run must not start while the first is in flight"
    );

    // The first finishes; the queued one takes the pane.
    ops.report_for_test(OpReport {
        id: first.unwrap(),
        sid: 7,
        name: "test.a",
        outcome: OpOutcome::Done,
    });
    let done = ops.pump(&host);
    assert_eq!(done.len(), 1, "the finished run is reported once");
    assert_eq!(ops.running(7), Some("test.b"));
}

/// A queue is for serialising, not for buffering.
#[test]
fn a_pane_queue_is_bounded() {
    let state = Arc::new(FakeState::default());
    let mut ops = PtyOps::new(Arc::new(FakeIo(Arc::clone(&state))) as Arc<dyn PtyIo>);
    for _ in 0..MAX_QUEUED_PER_PANE {
        assert!(ops.submit(1, PtyOp::new("test.x")).is_some());
    }
    assert!(
        ops.submit(1, PtyOp::new("test.x")).is_none(),
        "past the cap the caller is told, not silently queued forever"
    );
}

/// A late report from a superseded run must not free a pane that
/// someone else has since taken.
#[test]
fn a_stale_report_does_not_free_someone_elses_pane() {
    let state = Arc::new(FakeState::default());
    let mut ops = PtyOps::new(Arc::new(FakeIo(Arc::clone(&state))) as Arc<dyn PtyIo>);
    let host = FakePluginHost::default();
    let a = ops.submit(3, PtyOp::new("test.a")).unwrap();
    ops.pump(&host);
    ops.report_for_test(OpReport { id: a, sid: 3, name: "test.a", outcome: OpOutcome::Done });
    ops.pump(&host);
    let b = ops.submit(3, PtyOp::new("test.b")).unwrap();
    ops.pump(&host);
    assert_eq!(ops.running(3), Some("test.b"));
    // `a`'s report arriving again (a duplicate, a retry) must not
    // release the pane `b` is using.
    ops.report_for_test(OpReport { id: a, sid: 3, name: "test.a", outcome: OpOutcome::Done });
    ops.pump(&host);
    assert_eq!(ops.running(3), Some("test.b"), "still b's pane");
    assert_ne!(a, b);
}

/// A closed pane frees its slot and nothing queued for it starts.
#[test]
fn a_closed_pane_frees_its_slot_and_drops_its_queue() {
    let state = Arc::new(FakeState::default());
    let mut ops = PtyOps::new(Arc::new(FakeIo(Arc::clone(&state))) as Arc<dyn PtyIo>);
    let host = FakePluginHost::default();
    ops.submit(4, PtyOp::new("test.a")).unwrap();
    ops.submit(4, PtyOp::new("test.b")).unwrap();
    ops.submit(5, PtyOp::new("test.c")).unwrap();
    ops.pump(&host);
    assert!(ops.is_busy(4) && ops.is_busy(5));
    ops.retain(|sid| sid != 4);
    assert!(!ops.is_busy(4), "the closed pane's slot is free");
    ops.pump(&host);
    assert_eq!(ops.running(4), None, "its queued run did not start");
    assert_eq!(ops.running(5), Some("test.c"), "an open pane keeps its run");
    assert_eq!(host.begun.lock().unwrap().len(), 2);
}

/// Multi-line text goes as a paste, and oversized payloads are
/// refused rather than typed.
#[test]
fn text_is_delivered_as_a_paste_and_is_size_capped() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.msg").step(Step::paste("first line\nsecond line"));
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 100);
    assert_eq!(
        *state.pasted.lock().unwrap(),
        vec!["first line\nsecond line".to_string()],
        "text with newlines must not be typed a line at a time"
    );
    assert!(state.sent.lock().unwrap().is_empty(), "and not as raw input");

    let (state2, env2, host2) = setup();
    let huge = "x".repeat(MAX_PAYLOAD + 1);
    let finished: Arc<Mutex<Option<OpOutcome>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&finished);
    let mut r = OpRunner::new(PtyOp::new("test.huge").step(Step::paste(huge)), env2)
        .on_finish(move |_, o| *sink.lock().unwrap() = Some(o.clone()));
    run(&mut r, &host2, &state2, 100);
    r.on_end(&host2, EndReason::PluginRequested);
    assert!(state2.pasted.lock().unwrap().is_empty(), "nothing that big is delivered");
    assert!(
        matches!(finished.lock().unwrap().as_ref(), Some(OpOutcome::Failed { .. })),
        "and the caller is told why"
    );
}

/// A failed job ends the run before the steps after it — which is
/// what lets a script put a kill after it safely.
#[test]
fn a_failed_job_stops_the_run_before_the_kill() {
    let (state, env, host) = setup();
    state.alive.lock().unwrap().push(4242);
    let job = Job::new();
    let finished: Arc<Mutex<Option<OpOutcome>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&finished);
    let op = PtyOp::new("test.job")
        .step(Step::await_job(Arc::clone(&job)))
        .step(Step::terminate(fid(4242), libc::SIGTERM));
    let mut r = OpRunner::new(op, env).on_finish(move |_, o| *sink.lock().unwrap() = Some(o.clone()));
    run(&mut r, &host, &state, 500);
    assert!(state.signals.lock().unwrap().is_empty(), "nothing is killed while the job runs");
    job.finish(Err("no history".into()));
    job.finish(Ok(None)); // a second report must not overwrite the first
    run(&mut r, &host, &state, 500);
    r.on_end(&host, EndReason::PluginRequested);
    assert!(state.signals.lock().unwrap().is_empty(), "and nothing after it fails");
    assert!(matches!(
        finished.lock().unwrap().as_ref(),
        Some(OpOutcome::Failed { label: "await_job", err, .. }) if err == "no history"
    ));
}

/// A job that succeeds lets the run go on.
#[test]
fn a_finished_job_lets_the_run_continue() {
    let (state, env, host) = setup();
    state.alive.lock().unwrap().push(4242);
    let job = Job::new();
    job.finish(Ok(None));
    let op = PtyOp::new("test.job")
        .step(Step::await_job(job))
        .step(Step::terminate(fid(4242), libc::SIGTERM));
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 100);
    assert_eq!(state.signals.lock().unwrap().first(), Some(&(4242, libc::SIGTERM)));
}

/// A job's text is pasted; no text ends the run there, so nothing
/// after it (the Enter that would submit it) is sent.
#[test]
fn a_job_s_text_is_pasted_and_no_text_stops_the_run() {
    let (state, env, host) = setup();
    let job = Job::new();
    job.finish(Ok(Some("the message".into())));
    let op = PtyOp::new("test.pj").step(Step::paste_job(job)).step(Step::send(b"\r".to_vec()));
    run(&mut OpRunner::new(op, env), &host, &state, 100);
    assert_eq!(*state.pasted.lock().unwrap(), vec!["the message".to_string()]);
    assert_eq!(state.sent.lock().unwrap().len(), 1);

    let (state, env, host) = setup();
    let job = Job::new();
    job.finish(Ok(None));
    let op = PtyOp::new("test.pj").step(Step::paste_job(job)).step(Step::send(b"\r".to_vec()));
    run(&mut OpRunner::new(op, env), &host, &state, 100);
    assert!(state.pasted.lock().unwrap().is_empty());
    assert!(state.sent.lock().unwrap().is_empty(), "the Enter is not sent either");
}

/// A call's error fails the run.
#[test]
fn a_failing_call_fails_the_run() {
    let (state, env, host) = setup();
    let finished: Arc<Mutex<Option<OpOutcome>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&finished);
    let op = PtyOp::new("test.call")
        .step(Step::call(Arc::new(|| Err("disk full".into()))))
        .step(Step::send(b"x".to_vec()));
    let mut r = OpRunner::new(op, env).on_finish(move |_, o| *sink.lock().unwrap() = Some(o.clone()));
    run(&mut r, &host, &state, 100);
    r.on_end(&host, EndReason::PluginRequested);
    assert!(state.sent.lock().unwrap().is_empty());
    assert!(matches!(finished.lock().unwrap().as_ref(), Some(OpOutcome::Failed { err, .. }) if err == "disk full"));
}

/// Records which pane got a session, in order.  The op's name is
/// read out of the log line the service writes, so the test sees
/// what an operator would.
#[derive(Default)]
struct FakePluginHost {
    begun: Mutex<Vec<(u64, String)>>,
}

impl crate::plugins::PluginHost for FakePluginHost {
    fn state_dir(&self) -> Result<std::path::PathBuf, crate::plugins::PluginError> {
        Ok(std::env::temp_dir())
    }
    fn log(&self, _l: LogLevel, tag: &str, msg: &str) {
        if tag == "pty_op.started" {
            // "<name> on pane <sid> (id=N)"
            let name = msg.split_whitespace().next().unwrap_or("?").to_string();
            let sid = msg
                .split("on pane ")
                .nth(1)
                .and_then(|r| r.split_whitespace().next())
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0);
            self.begun.lock().unwrap().push((sid, name));
        }
    }
    fn begin_pane_session(
        &self,
        _sid: u64,
        _session: Box<dyn PaneSession>,
    ) -> Result<(), crate::plugins::PluginError> {
        Ok(())
    }
}

/// The queue must not need a plugin's permissions to start a run.
///
/// This shipped: `pump` went through `PluginHost::begin_pane_session`,
/// which checks the *current plugin's* permissions — so the very
/// first CLI delivery was refused by the supervisor's own gate with
/// `missing permission`, and the text never reached the pane.  The
/// queue's owner is the authority; starting a run is its business.
#[test]
fn the_queue_starts_runs_through_its_owner_not_a_plugin_gate() {
    struct Owner {
        started: Mutex<Vec<u64>>,
    }
    impl OpHost for Owner {
        fn begin(&self, sid: u64, _s: Box<dyn PaneSession>) -> Result<(), String> {
            self.started.lock().unwrap().push(sid);
            Ok(())
        }
        fn log(&self, _l: LogLevel, _t: &str, _m: &str) {}
    }
    let state = Arc::new(FakeState::default());
    let mut ops = PtyOps::new(Arc::new(FakeIo(Arc::clone(&state))) as Arc<dyn PtyIo>);
    let owner = Owner { started: Mutex::new(Vec::new()) };
    ops.submit(390, PtyOp::new("cli.send").step(Step::paste("hi")));
    ops.pump(&owner);
    assert_eq!(*owner.started.lock().unwrap(), vec![390]);
    assert!(ops.is_busy(390));
}

/// A fake process id: the fake env matches on the pid alone.
fn fid(pid: i32) -> ProcId {
    ProcId { pid, start_us: 0 }
}

#[derive(Default)]
pub(super) struct FakeState {
    now_ms: Mutex<u64>,
    alive: Mutex<Vec<i32>>,
    present: Mutex<Vec<i32>>,
    out_len: Mutex<u64>,
    signals: Mutex<Vec<(i32, i32)>>,
    sent: Mutex<Vec<Vec<u8>>>,
    holds: Mutex<Vec<bool>>,
    pasted: Mutex<Vec<String>>,
    mouse_resets: Mutex<Vec<u64>>,
    programs: Mutex<Vec<String>>,
    /// `(seq, flags)` the pane's region would report, or `None`
    /// for a pane whose region cannot be read.
    modes: Mutex<Option<(u64, u32)>>,
    /// What the pane's screen would show.
    screen: Mutex<String>,
    /// What this pane's agent reports, if it reports at all.
    ///
    /// `None` -- the default -- is a pane with no receipt channel,
    /// which is every pane that existed before there was one and
    /// every pane running something that is not an agent. Those
    /// read the screen, as they always did.
    receipt: Mutex<Option<bool>>,
    /// What the pane has drawn lately.
    tail: Mutex<Vec<u8>>,
    /// What `signal` answers instead of delivering, per signal:
    /// the pid naming another process by now, or one we may not
    /// signal.  A signal not listed is delivered.
    refuse: Mutex<Vec<(i32, SignalError)>>,
}

struct FakeIo(Arc<FakeState>);
impl PtyIo for FakeIo {
    fn send(&self, _sid: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.0.sent.lock().unwrap().push(bytes.to_vec());
        Ok(())
    }
    fn hold(&self, _sid: u64, on: bool) -> std::io::Result<()> {
        self.0.holds.lock().unwrap().push(on);
        Ok(())
    }
    fn paste(&self, _sid: u64, text: &str) -> std::io::Result<()> {
        self.0.pasted.lock().unwrap().push(text.to_string());
        Ok(())
    }
    fn program_input(&self, _sid: u64, what: Typed<'_>) -> std::io::Result<()> {
        self.0.programs.lock().unwrap().push(format!("{what:?}"));
        Ok(())
    }
    fn reset_mouse_reporting(&self, sid: u64) -> std::io::Result<()> {
        self.0.mouse_resets.lock().unwrap().push(sid);
        Ok(())
    }
}

impl OpEnv for FakeEnv {
    fn submit_receipt(&self, _sid: u64, _text: &str) -> Option<bool> {
        *self.state.receipt.lock().unwrap()
    }
    fn io(&self) -> &Arc<dyn PtyIo> {
        &self.io
    }
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_millis(*self.state.now_ms.lock().unwrap())
    }
    fn signal(&self, id: ProcId, sig: i32) -> Result<(), SignalError> {
        self.state.signals.lock().unwrap().push((id.pid, sig));
        if let Some((_, e)) = self.state.refuse.lock().unwrap().iter().find(|(s, _)| *s == sig) {
            return Err(*e);
        }
        if sig == libc::SIGKILL {
            self.state.alive.lock().unwrap().retain(|p| *p != id.pid);
        }
        Ok(())
    }
    fn pid_alive(&self, id: ProcId) -> bool {
        self.state.alive.lock().unwrap().contains(&id.pid)
    }
    fn output_tail(&self, _sid: u64, _bytes: usize) -> Vec<u8> {
        self.state.tail.lock().unwrap().clone()
    }
    fn pane_screen(&self, _sid: u64) -> Option<String> {
        Some(self.state.screen.lock().unwrap().clone())
    }
    fn pane_unsent(&self, sid: u64) -> Option<String> {
        self.pane_screen(sid).as_deref().and_then(crate::plugins::autorun::unsent_line)
    }
    fn pane_modes(&self, _sid: u64) -> Option<(u64, u32)> {
        *self.state.modes.lock().unwrap()
    }
    fn find_descendant(&self, _under: i32, _pred: fn(&pidtree::ProcRow) -> bool) -> Option<ProcId> {
        self.state.present.lock().unwrap().first().map(|&pid| fid(pid))
    }
    // the processes these scripts start are the shell's jobs, in front
    fn find_in_front(&self, under: i32, pred: fn(&pidtree::ProcRow) -> bool) -> Option<ProcId> {
        self.find_descendant(under, pred)
    }
    // the shell under these scripts is a fiction that is always at
    // its prompt; the real check is tested on real shells
    fn front(&self, _shell: i32) -> front::Front {
        front::Front::Shell
    }
    fn output_len(&self, _sid: u64) -> u64 {
        *self.state.out_len.lock().unwrap()
    }
}

pub(super) struct Host {
    ended: Mutex<bool>,
    pub(super) badges: Mutex<Vec<String>>,
}
impl PaneSessionHost for Host {
    fn shelld_session_id(&self) -> u64 {
        1
    }
    fn end(&self) {
        *self.ended.lock().unwrap() = true;
    }
    fn set_badge(&self, t: &str) {
        self.badges.lock().unwrap().push(t.into());
    }
    fn log(&self, _l: LogLevel, _t: &str, _m: &str) {}
}

pub(super) fn setup() -> (Arc<FakeState>, Box<dyn OpEnv>, Host) {
    let state = Arc::new(FakeState::default());
    let env = FakeEnv {
        io: Arc::new(FakeIo(Arc::clone(&state))) as Arc<dyn PtyIo>,
        state: Arc::clone(&state),
    };
    let host = Host { ended: Mutex::new(false), badges: Mutex::new(Vec::new()) };
    (state, Box::new(env), host)
}

fn advance(state: &FakeState, ms: u64) {
    *state.now_ms.lock().unwrap() += ms;
}

/// Ticks the runner the way the host does, advancing the clock a
/// frame at a time.  Returns when the run ends or the budget runs
/// out.
pub(super) fn run(r: &mut OpRunner, host: &Host, state: &FakeState, ms: u64) {
    for _ in 0..(ms / 16) {
        if *host.ended.lock().unwrap() {
            return;
        }
        r.on_tick(host);
        advance(state, 16);
    }
}

/// Readiness is the program's own declaration, read off the byte
/// stream rather than off a published frame.
///
/// The frame cannot be the evidence: this step runs inside a
/// script that holds the screen, so nothing is republished and the
/// sequence number never moves.  Every switch on 2026-09-30 sat
/// out its whole budget with both mode flags already set, waiting
/// for a newer frame that a hold guarantees will not come.
#[test]
fn readiness_comes_from_the_bytes_not_the_frame() {
    let (state, env, host) = setup();
    // A shell prompt: bracketed paste on, no alternate screen.
    *state.tail.lock().unwrap() = b"\x1b[?2004h$ ".to_vec();
    *state.out_len.lock().unwrap() = 10;
    let mut r = OpRunner::new(
        PtyOp::new("test.ready")
            .step(Step::await_tui_ready().timeout(Duration::from_millis(500))),
        env,
    );
    run(&mut r, &host, &state, 200);
    assert!(!*host.ended.lock().unwrap(), "a shell is not a full-screen program");

    // The program takes the terminal.
    *state.tail.lock().unwrap() =
        b"\x1b[?2004h$ claude\r\n\x1b[?1049h\x1b[?2004h".to_vec();
    *state.out_len.lock().unwrap() = 40;
    run(&mut r, &host, &state, 400);
    assert!(*host.ended.lock().unwrap(), "both declarations are the answer");
}

/// The composer carry is for the person's sentence, and marspot's
/// own words are not theirs.
///
/// This is how one sentence became immortal: an older build typed
/// it into the composer, where it stuck. Every switch after that
/// read it as unsent text, held it, and put it back -- five panes
/// carrying the same 34 characters for a day. The sentence had
/// already moved to the command line by then; what kept showing it
/// in the input box was the feature meant to protect what the
/// person was writing.
#[test]
fn the_composer_carry_does_not_carry_what_marspot_said() {
    let (state, env, host) = setup();
    let ours = "额度恢复了，如果有刚被打断的工作请继续，如果是 idle 状态请保持";
    *state.screen.lock().unwrap() = format!("❯ {ours}\n");
    let held = Arc::new(Job::new());
    let mut r = OpRunner::new(
        PtyOp::new("test.disown")
            .step(Step::capture_composer(Arc::clone(&held)).disowning(ours)),
        env,
    );
    run(&mut r, &host, &state, 64);
    assert_eq!(
        held.text(),
        None,
        "a line this run is responsible for is not the person's to carry"
    );
}

/// And what the person typed after it still travels.
#[test]
fn what_the_person_added_after_it_is_still_theirs() {
    let (state, env, host) = setup();
    let ours = "say this";
    *state.screen.lock().unwrap() = format!("❯ {ours} and then my own words\n");
    let held = Arc::new(Job::new());
    let mut r = OpRunner::new(
        PtyOp::new("test.disown2")
            .step(Step::capture_composer(Arc::clone(&held)).disowning(ours)),
        env,
    );
    run(&mut r, &host, &state, 64);
    assert_eq!(held.text().as_deref(), Some("and then my own words"));
}

/// A message another session delivered into the input box is not
/// the person's half-finished sentence, and carrying it would hand
/// their cursor someone else's words -- then do it again on every
/// switch, because the carry writes it back in.
#[test]
fn a_message_delivered_into_the_composer_is_not_carried() {
    let (state, env, host) = setup();
    *state.screen.lock().unwrap() =
        "❯ Message from @smix-30: firefly /root/work/smix cleaned; disk 78% → 59%.\n"
            .to_string();
    let held = Arc::new(Job::new());
    let mut r = OpRunner::new(
        PtyOp::new("test.delivered").step(Step::capture_composer(Arc::clone(&held))),
        env,
    );
    run(&mut r, &host, &state, 64);
    assert_eq!(held.text(), None, "a delivery is not something the person typed");
}

/// And a sentence that merely begins with the same words is theirs.
#[test]
fn a_sentence_about_a_message_is_still_the_persons() {
    let (state, env, host) = setup();
    *state.screen.lock().unwrap() =
        "❯ Message from the deploy box never arrived\n".to_string();
    let held = Arc::new(Job::new());
    let mut r = OpRunner::new(
        PtyOp::new("test.not_delivered").step(Step::capture_composer(Arc::clone(&held))),
        env,
    );
    run(&mut r, &host, &state, 64);
    assert_eq!(
        held.text().as_deref(),
        Some("Message from the deploy box never arrived")
    );
}

/// A composer holding only the person's own text is untouched.
#[test]
fn a_sentence_that_is_not_ours_is_carried_whole() {
    let (state, env, host) = setup();
    *state.screen.lock().unwrap() = "❯ half a thought\n".to_string();
    let held = Arc::new(Job::new());
    let mut r = OpRunner::new(
        PtyOp::new("test.theirs")
            .step(Step::capture_composer(Arc::clone(&held)).disowning("something else")),
        env,
    );
    run(&mut r, &host, &state, 64);
    assert_eq!(held.text().as_deref(), Some("half a thought"));
}

/// A program quicker than the tick still counts as having drawn.
///
/// The readiness probe reads the bytes a line produced, and it used
/// to start reading when it first ran -- so a program that took the
/// terminal in the gap between the send and that first tick wrote
/// its declaration into a window nobody was looking at. The step
/// then waited its whole budget and reported a pane that drew
/// nothing, while the pane was showing a full screen. Measured end
/// to end on 2026-10-01: painted at :31, the step opened at :32,
/// gave up thirty seconds later.
#[test]
fn a_program_that_drew_before_the_step_opened_still_counts() {
    let (state, env, host) = setup();
    let mut r = OpRunner::new(
        PtyOp::new("test.quick")
            .step(Step::send(b"claude\r".to_vec()).named("resume"))
            .step(Step::await_tui_ready().timeout(Duration::from_millis(500))),
        env,
    );
    // The send goes out and the readiness step opens on an empty
    // stream. The program then takes the terminal and goes quiet,
    // which under the old rule was a declaration written into a
    // window nobody was reading.
    // Exactly one tick, so the send is out and the readiness step
    // has not opened yet. The bytes arrive in that gap -- which is
    // the whole case: under the old rule the step then started
    // reading from after them and saw an empty stream forever.
    run(&mut r, &host, &state, 16);
    *state.tail.lock().unwrap() = b"\x1b[?1049h\x1b[?2004h".to_vec();
    *state.out_len.lock().unwrap() = 20;
    run(&mut r, &host, &state, 300);
    assert!(
        *host.ended.lock().unwrap(),
        "what the line produced is the evidence, whenever it arrived"
    );
}

/// The same gap, one step later: the floor on bytes drawn counts
/// what the line produced, not what arrived after the step opened.
///
/// A program that paints its first screen and then waits is
/// finished before a later step gets a turn, and that step's own
/// reckoning says nothing was drawn -- so a floor of two kilobytes
/// can never be reached and the run gives up on a pane that is
/// showing a full screen. Measured end to end: 4604 bytes in, and
/// the step that wanted 2048 of them timed out after thirty
/// seconds.
#[test]
fn a_first_frame_painted_before_the_step_opened_counts_too() {
    let (state, env, host) = setup();
    let mut r = OpRunner::new(
        PtyOp::new("test.painted")
            .step(Step::send(b"claude\r".to_vec()).named("resume"))
            // The step in between is the one that consumes the
            // paint: it is satisfied by the same bytes, and the
            // frame wait then opens on the far side of them.
            .step(Step::await_tui_ready().timeout(Duration::from_millis(400)))
            .step(
                Step::await_quiet(Duration::from_millis(50))
                    .after_bytes(2048)
                    .timeout(Duration::from_millis(600)),
            ),
        env,
    );
    run(&mut r, &host, &state, 16);
    // The whole screen, at once.
    *state.tail.lock().unwrap() = b"\x1b[?1049h\x1b[?2004h".to_vec();
    *state.out_len.lock().unwrap() = 4604;
    run(&mut r, &host, &state, 500);
    assert!(
        *host.ended.lock().unwrap(),
        "the floor is about what the line produced, not about when this step woke up"
    );
}

/// And a floor that is genuinely not reached still holds.
#[test]
fn a_program_that_barely_painted_does_not_pass_the_floor() {
    let (state, env, host) = setup();
    let mut r = OpRunner::new(
        PtyOp::new("test.thin")
            .step(Step::send(b"claude\r".to_vec()).named("resume"))
            .step(
                Step::await_quiet(Duration::from_millis(50))
                    .after_bytes(2048)
                    .timeout(Duration::from_millis(300)),
            ),
        env,
    );
    run(&mut r, &host, &state, 16);
    *state.out_len.lock().unwrap() = 12;
    run(&mut r, &host, &state, 400);
    assert!(
        matches!(r.finished, Some(OpOutcome::TimedOut { .. })),
        "twelve bytes is not a first frame: {:?}",
        r.finished
    );
}

/// And a frame that never moves does not stop it.
#[test]
fn a_held_screen_does_not_starve_the_wait() {
    let (state, env, host) = setup();
    // `pane_modes` pinned: this is what a hold looks like from the
    // shm side — the same frame, forever.
    *state.modes.lock().unwrap() = Some((7, 0x25));
    let mut r = OpRunner::new(
        PtyOp::new("test.held")
            .step(Step::await_tui_ready().timeout(Duration::from_millis(500))),
        env,
    );
    // Nothing written yet when the step opens; the program writes
    // afterwards, which is the only thing that still moves under a
    // hold.
    run(&mut r, &host, &state, 32);
    *state.tail.lock().unwrap() = b"\x1b[?1049h\x1b[?2004h".to_vec();
    *state.out_len.lock().unwrap() = 20;
    run(&mut r, &host, &state, 300);
    assert!(
        *host.ended.lock().unwrap(),
        "the wait must not depend on a frame the hold is preventing"
    );
}

/// The text leaves first; the return waits for the text to come
/// back and the repaint that carried it to finish.
///
/// Three attempts got to this.  Glued together, the composer reads
/// the return as part of the pasted block and turns it into a
/// second line.  "It wrote something back" was not evidence — that
/// interface repaints on its own.  Adding a quiet window fixed
/// four panes out of five; the fifth was mid-task, painted 104
/// bytes of something else, and went quiet without ever having
/// drawn the paste.  So the gate is the text itself.
#[test]
fn the_return_waits_for_the_text_to_come_back() {
    use marspot_term::grid_shm::{FLAG_ALT_SCREEN, FLAG_BRACKETED_PASTE};
    let (state, env, host) = setup();
    *state.modes.lock().unwrap() = Some((1, FLAG_ALT_SCREEN | FLAG_BRACKETED_PASTE));
    let mut r = OpRunner::new(PtyOp::new("test.submit").step(Step::submit("carry on")), env);

    run(&mut r, &host, &state, 32);
    assert_eq!(
        state.sent.lock().unwrap().clone(),
        vec![b"\x1b[200~carry on\x1b[201~".to_vec()],
        "the paste goes out on its own, wrapped"
    );

    // The pane paints, and goes quiet, without the text in it —
    // the case that got through the previous gate.
    *state.tail.lock().unwrap() = b"\x1b[2K\x1b[38;5;8m  Searched for 1 pattern".to_vec();
    *state.out_len.lock().unwrap() = 104;
    run(&mut r, &host, &state, SUBMIT_QUIET.as_millis() as u64 * 3);
    assert_eq!(
        state.sent.lock().unwrap().len(),
        1,
        "a quiet pane that never drew the paste has not consumed it"
    );

    // Now it gets round to its input queue.
    *state.tail.lock().unwrap() = b"\x1b[2K> carry on".to_vec();
    *state.out_len.lock().unwrap() = 300;
    run(&mut r, &host, &state, 32);
    assert_eq!(state.sent.lock().unwrap().len(), 1, "and the repaint has to finish");

    run(&mut r, &host, &state, SUBMIT_QUIET.as_millis() as u64 + 64);
    let sent = state.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[1], b"\r".to_vec(), "and it is a return by itself");

    // The composer came back empty, so the return landed.
    *state.screen.lock().unwrap() = "\u{276f}\n".to_string();
    run(&mut r, &host, &state, SUBMIT_VERIFY_AFTER.as_millis() as u64 + 64);
    assert!(*host.ended.lock().unwrap());
    assert_eq!(state.sent.lock().unwrap().len(), 2, "one return was enough");
}

/// A return that became a newline gets another one.
///
/// Every gate before this is a guess about when the composer is
/// ready, and three of them have been wrong in a row.  This one
/// reads the pane: if the line is still on the last prompt, the
/// return did not submit it.
#[test]
fn a_return_that_was_eaten_is_sent_again() {
    use marspot_term::grid_shm::{FLAG_ALT_SCREEN, FLAG_BRACKETED_PASTE};
    let (state, env, host) = setup();
    *state.modes.lock().unwrap() = Some((1, FLAG_ALT_SCREEN | FLAG_BRACKETED_PASTE));
    let mut r = OpRunner::new(
        PtyOp::new("test.submit")
            .step(Step::submit("carry on").timeout(Duration::from_secs(60))),
        env,
    );
    // Paste, show the text, settle, first return.
    run(&mut r, &host, &state, 32);
    *state.tail.lock().unwrap() = b"\x1b[2K> carry on".to_vec();
    *state.out_len.lock().unwrap() = 300;
    run(&mut r, &host, &state, SUBMIT_QUIET.as_millis() as u64 + 96);
    assert_eq!(state.sent.lock().unwrap().len(), 2, "the first return");

    // …and the line is still sitting there.
    *state.screen.lock().unwrap() = "some output\n\u{276f} carry on\n".to_string();
    run(&mut r, &host, &state, SUBMIT_VERIFY_AFTER.as_millis() as u64 + 96);
    assert_eq!(state.sent.lock().unwrap().len(), 3, "a second return goes out");

    // Now it lands.
    *state.screen.lock().unwrap() = "\u{276f} carry on\nsome output\n\u{276f}\n".to_string();
    run(&mut r, &host, &state, SUBMIT_VERIFY_AFTER.as_millis() as u64 + 96);
    assert!(*host.ended.lock().unwrap(), "the last prompt is empty, so it went");
    assert_eq!(state.sent.lock().unwrap().len(), 3, "and no more are sent");
}

/// A pane that has drawn nothing is not typed into.
///
/// `or_late` is for a program that is slow. One that has produced
/// zero bytes has not started, and going on means pasting into
/// something that is not reading: the return lands as a newline
/// and the sentence sits in the composer. That is what a machine
/// at load 513 produced on 2026-10-01 -- `drew 0B` after three
/// seconds, and three returns later a line left for the person.
#[test]
fn a_pane_that_drew_nothing_gets_longer_before_anything_is_typed() {
    use marspot_term::grid_shm::{FLAG_ALT_SCREEN, FLAG_BRACKETED_PASTE};
    let (state, env, host) = setup();
    *state.modes.lock().unwrap() = Some((1, FLAG_ALT_SCREEN | FLAG_BRACKETED_PASTE));
    let mut r = OpRunner::new(
        PtyOp::new("test.late")
            .step(Step::await_quiet(Duration::from_millis(100))
                .timeout(Duration::from_millis(500))
                .or_late())
            .step(Step::paste("something")),
        env,
    );
    // Nothing is ever drawn. Past the step's own timeout, and
    // still nothing has been typed.
    run(&mut r, &host, &state, 2_000);
    assert!(
        state.pasted.lock().unwrap().is_empty(),
        "four times the timeout is still not a reason to type at a pane that has drawn nothing"
    );
    assert!(!*host.ended.lock().unwrap(), "and it is still waiting, not finished");

    // Past the grace, it gives up rather than typing.
    run(&mut r, &host, &state, 5_000);
    assert!(*host.ended.lock().unwrap(), "it has to stop eventually");
    assert!(
        state.pasted.lock().unwrap().is_empty(),
        "and it must not have typed on the way out"
    );
}

/// A pane that drew *something* is merely slow, and going on is
/// what `or_late` is for.
#[test]
fn a_pane_that_drew_something_is_only_slow() {
    use marspot_term::grid_shm::{FLAG_ALT_SCREEN, FLAG_BRACKETED_PASTE};
    let (state, env, host) = setup();
    *state.modes.lock().unwrap() = Some((1, FLAG_ALT_SCREEN | FLAG_BRACKETED_PASTE));
    let mut r = OpRunner::new(
        PtyOp::new("test.late")
            .step(Step::await_quiet(Duration::from_millis(100))
                .timeout(Duration::from_millis(500))
                .or_late())
            .step(Step::paste("something")),
        env,
    );
    // Drawing the whole time, so `await_quiet` is never satisfied
    // and the step runs out -- which is the case `or_late` is for.
    // Set before the run it would be zero against `entered_len`,
    // and set once it would simply go quiet and succeed.
    for _ in 0..80 {
        *state.out_len.lock().unwrap() += 100;
        r.on_tick(&host);
        advance(&state, 16);
        if *host.ended.lock().unwrap() {
            break;
        }
    }
    assert_eq!(
        *state.pasted.lock().unwrap(),
        vec!["something".to_string()],
        "a slow pane still gets its line"
    );
}

/// A half-written sentence survives the agent being restarted.
///
/// Switching profiles kills claude and starts another one. What
/// the person had typed and not sent lives only in the old
/// process's input box, so it has to be read off the screen before
/// the signal and handed back after the new one is reading input.
#[test]
fn what_the_person_was_typing_survives_the_restart() {
    use marspot_term::grid_shm::{FLAG_ALT_SCREEN, FLAG_BRACKETED_PASTE};
    let (state, env, host) = setup();
    *state.modes.lock().unwrap() = Some((1, FLAG_ALT_SCREEN | FLAG_BRACKETED_PASTE));
    *state.screen.lock().unwrap() = "a reply\n\u{276f} 我写了一半的句子\n".to_string();
    let held = Job::new();
    let mut r = OpRunner::new(
        PtyOp::new("test.cycle")
            .step(Step::capture_composer(Arc::clone(&held)))
            // Stands in for kill / restart / await-ready.
            .step(Step::settle(Duration::from_millis(10)))
            .step(Step::restore_composer(Arc::clone(&held)))
            // Only reached when nothing was held.
            .step(Step::paste("我们要说的话")),
        env,
    );
    // One tick: the capture completes and the settle after it
    // starts waiting. (`run` advances in 16 ms steps, so anything
    // under that runs nothing at all.)
    run(&mut r, &host, &state, 16);
    assert_eq!(
        held.text().as_deref(),
        Some("我写了一半的句子"),
        "read before the agent is taken down, or there is nothing left to read"
    );
    // Now the agent has restarted and its box is empty.
    *state.screen.lock().unwrap() = "\u{276f}\n".to_string();
    run(&mut r, &host, &state, 2_000);

    let pasted = state.pasted.lock().unwrap().clone();
    assert_eq!(
        pasted,
        vec!["我写了一半的句子".to_string()],
        "their sentence goes back, and ours is not stapled on: {pasted:?}"
    );
    assert!(*host.ended.lock().unwrap(), "and the run ends there");
}

/// An empty composer is the ordinary case and changes nothing.
#[test]
fn an_empty_composer_lets_the_run_carry_on() {
    use marspot_term::grid_shm::{FLAG_ALT_SCREEN, FLAG_BRACKETED_PASTE};
    let (state, env, host) = setup();
    *state.modes.lock().unwrap() = Some((1, FLAG_ALT_SCREEN | FLAG_BRACKETED_PASTE));
    *state.screen.lock().unwrap() = "a reply\n\u{276f}\n".to_string();
    let held = Job::new();
    let mut r = OpRunner::new(
        PtyOp::new("test.cycle")
            .step(Step::capture_composer(Arc::clone(&held)))
            .step(Step::restore_composer(Arc::clone(&held)))
            .step(Step::paste("我们要说的话")),
        env,
    );
    run(&mut r, &host, &state, 2_000);
    assert_eq!(
        *state.pasted.lock().unwrap(),
        vec!["我们要说的话".to_string()],
        "nothing was held, so the thing we meant to say is said"
    );
}

/// A screen that cannot be read holds nothing.
///
/// Not knowing what was in the box is not permission to invent
/// something to put in it: a line the person can retype is a
/// smaller loss than text appearing in their composer from
/// nowhere.
#[test]
fn an_unreadable_screen_holds_nothing() {
    use marspot_term::grid_shm::{FLAG_ALT_SCREEN, FLAG_BRACKETED_PASTE};
    let (state, env, host) = setup();
    *state.modes.lock().unwrap() = Some((1, FLAG_ALT_SCREEN | FLAG_BRACKETED_PASTE));
    *state.screen.lock().unwrap() = "no composer here at all\n".to_string();
    let held = Job::new();
    let mut r = OpRunner::new(
        PtyOp::new("test.cycle")
            .step(Step::capture_composer(Arc::clone(&held)))
            .step(Step::restore_composer(Arc::clone(&held)))
            .step(Step::paste("我们要说的话")),
        env,
    );
    run(&mut r, &host, &state, 2_000);
    assert_eq!(*state.pasted.lock().unwrap(), vec!["我们要说的话".to_string()]);
}

/// A pane that reports its own submit is believed, and its screen
/// is not consulted.
///
/// The screen here says the line is still sitting in the composer
/// -- the exact picture that makes every other test send another
/// return. The receipt outranks it, because the receipt came from
/// the program that did the submitting and the screen is a guess
/// about what that program drew.
#[test]
fn a_reported_submit_is_believed_over_the_screen() {
    use marspot_term::grid_shm::{FLAG_ALT_SCREEN, FLAG_BRACKETED_PASTE};
    let (state, env, host) = setup();
    *state.modes.lock().unwrap() = Some((1, FLAG_ALT_SCREEN | FLAG_BRACKETED_PASTE));
    let mut r = OpRunner::new(
        PtyOp::new("test.submit")
            .step(Step::submit("carry on").timeout(Duration::from_secs(60))),
        env,
    );
    run(&mut r, &host, &state, 32);
    *state.tail.lock().unwrap() = b"\x1b[2K> carry on".to_vec();
    *state.out_len.lock().unwrap() = 300;
    run(&mut r, &host, &state, SUBMIT_QUIET.as_millis() as u64 + 96);
    assert_eq!(state.sent.lock().unwrap().len(), 2, "paste and one return");

    // The screen says it is stuck; the agent says it submitted.
    *state.screen.lock().unwrap() = "\u{276f} carry on\n".to_string();
    *state.receipt.lock().unwrap() = Some(true);
    run(&mut r, &host, &state, SUBMIT_VERIFY_AFTER.as_millis() as u64 + 96);
    assert!(*host.ended.lock().unwrap(), "the receipt settles it");
    assert_eq!(
        state.sent.lock().unwrap().len(),
        2,
        "and no second return goes out on top of a submit that happened"
    );
}

/// **The acceptance for the whole receipt design.**
///
/// A pane that has a receipt channel and never reports must end up
/// saying so. Before receipts the verification could not fail at
/// all -- `.stuck` had never fired once in the file's history,
/// because the branch that declared success always won first. Cut
/// the channel and this run has to end in returns exhausted, not
/// in a quiet `landed`.
#[test]
fn a_channel_that_never_reports_ends_in_giving_up_not_in_success() {
    use marspot_term::grid_shm::FLAG_BRACKETED_PASTE;
    let (state, env, host) = setup();
    *state.modes.lock().unwrap() = Some((1, FLAG_BRACKETED_PASTE));
    let mut r = OpRunner::new(
        PtyOp::new("test.submit")
            .step(Step::submit("carry on").timeout(Duration::from_secs(120))),
        env,
    );
    run(&mut r, &host, &state, 32);
    *state.tail.lock().unwrap() = b"> carry on".to_vec();
    *state.out_len.lock().unwrap() = 300;
    // The channel is there and silent, and the screen says the
    // line went: an empty composer below the message. Without the
    // rule above, that screen ends the run in `landed` and the
    // receipt buys nothing.
    *state.receipt.lock().unwrap() = Some(false);
    *state.screen.lock().unwrap() =
        "\u{276f} carry on\n  …a reply…\n\u{276f}\n".to_string();
    run(&mut r, &host, &state, 20_000);
    let sent = state.sent.lock().unwrap().clone();
    assert_eq!(
        sent.len(),
        1 + SUBMIT_MAX_RETURNS as usize,
        "every return is spent before it gives up: {sent:?}"
    );
    assert!(*host.ended.lock().unwrap(), "and it ends rather than hanging");
}

/// A pane with no channel at all still reads the screen.
///
/// Receipts are an addition, not a replacement: a plain shell, an
/// older agent, or a pane that started before any of this still
/// has to submit exactly as it did.
#[test]
fn a_pane_without_a_channel_still_goes_by_the_screen() {
    use marspot_term::grid_shm::{FLAG_ALT_SCREEN, FLAG_BRACKETED_PASTE};
    let (state, env, host) = setup();
    *state.modes.lock().unwrap() = Some((1, FLAG_ALT_SCREEN | FLAG_BRACKETED_PASTE));
    let mut r = OpRunner::new(
        PtyOp::new("test.submit")
            .step(Step::submit("carry on").timeout(Duration::from_secs(60))),
        env,
    );
    run(&mut r, &host, &state, 32);
    *state.tail.lock().unwrap() = b"\x1b[2K> carry on".to_vec();
    *state.out_len.lock().unwrap() = 300;
    run(&mut r, &host, &state, SUBMIT_QUIET.as_millis() as u64 + 96);
    // No receipt channel (the default), and an empty composer
    // below the message: the screen says it went.
    assert_eq!(*state.receipt.lock().unwrap(), None);
    *state.screen.lock().unwrap() =
        "\u{276f} carry on\n  …a reply…\n\u{276f}\n".to_string();
    run(&mut r, &host, &state, SUBMIT_VERIFY_AFTER.as_millis() as u64 + 96);
    assert!(*host.ended.lock().unwrap(), "the screen is still evidence when it is all there is");
    assert_eq!(state.sent.lock().unwrap().len(), 2, "one paste, one return");
}

/// Three returns, then it is the person's.
#[test]
fn it_stops_after_three_returns_rather_than_hammering() {
    use marspot_term::grid_shm::FLAG_BRACKETED_PASTE;
    let (state, env, host) = setup();
    *state.modes.lock().unwrap() = Some((1, FLAG_BRACKETED_PASTE));
    let mut r = OpRunner::new(
        PtyOp::new("test.submit")
            .step(Step::submit("carry on").timeout(Duration::from_secs(120))),
        env,
    );
    run(&mut r, &host, &state, 32);
    *state.tail.lock().unwrap() = b"> carry on".to_vec();
    *state.out_len.lock().unwrap() = 300;
    // Never leaves the composer, whatever is sent.
    *state.screen.lock().unwrap() = "\u{276f} carry on\n".to_string();
    run(&mut r, &host, &state, 20_000);
    let sent = state.sent.lock().unwrap().clone();
    assert_eq!(
        sent.len(),
        1 + SUBMIT_MAX_RETURNS as usize,
        "one paste and three returns, then stop: {sent:?}"
    );
    assert!(*host.ended.lock().unwrap(), "and the op ends rather than hanging");
}

/// A message already sent is not a message still in the composer.
///
/// The interface marks both with the same glyph, so "the text is on
/// screen" would say every successful submit had failed.  The rule
/// is the LAST prompt, which is the composer.
#[test]
fn a_sent_message_is_not_mistaken_for_a_stuck_one() {
    let sent = "\u{276f} carry on\n  …a reply…\n\u{276f}\n";
    assert!(!line_is_still_in_the_composer(sent, "carry on"));

    let stuck = "  …a reply…\n\u{276f} carry on\n";
    assert!(line_is_still_in_the_composer(stuck, "carry on"));
}

/// Every composer this runs against, read off the real panes.
///
/// Claude Code draws U+276F and Codex draws U+203A. Only the first
/// was in the list, so for every Codex pane the check found no
/// prompt, took the branch below, and reported `landed` -- the
/// verification could not fail there, which is worse than not
/// having it.
#[test]
fn each_agents_prompt_character_is_recognised() {
    for prompt in ['\u{276f}', '\u{203a}', '>'] {
        let stuck = format!("  …a reply…\n{prompt} carry on\n");
        assert!(
            line_is_still_in_the_composer(&stuck, "carry on"),
            "{prompt:?} (U+{:04X}) is a composer and this line is unsent",
            prompt as u32
        );
        let sent = format!("{prompt} carry on\n  …a reply…\n{prompt}\n");
        assert!(
            !line_is_still_in_the_composer(&sent, "carry on"),
            "{prompt:?}: an empty composer below the message means it was sent"
        );
    }
}

/// A screen with no composer on it means "I cannot tell", and that
/// has to read as *still there*.
///
/// It used to read as sent. Every way of failing -- a pane
/// mid-repaint, an unknown prompt character, a composer scrolled
/// away -- produced the same `false` as a real submit, so the op
/// logged `landed` and the text stayed in the box. On 2026-10-01
/// three panes sat with an unsent line while the log said all of
/// them had landed, and `.stuck` had never fired once in the
/// history of the file: the giving-up branch was unreachable
/// because the succeeding one always won.
#[test]
fn a_screen_we_cannot_read_is_not_evidence_of_success() {
    assert!(
        line_is_still_in_the_composer("just output\n", "carry on"),
        "no composer on screen is not knowing, and not knowing is not success"
    );
    assert!(
        line_is_still_in_the_composer("", "carry on"),
        "an empty screen is the same kind of not knowing"
    );
}

/// A program that never gets to it still gets its return.
///
/// Late is the safe direction: a sentence that lands slowly is a
/// slow sentence, and one that never lands is the op hanging on a
/// pane nobody is watching.
#[test]
fn a_pane_that_never_shows_the_text_gets_its_return_at_the_grace() {
    use marspot_term::grid_shm::FLAG_BRACKETED_PASTE;
    let (state, env, host) = setup();
    *state.modes.lock().unwrap() = Some((1, FLAG_BRACKETED_PASTE));
    let mut r = OpRunner::new(
        PtyOp::new("test.submit")
            .step(Step::submit("carry on").timeout(SUBMIT_ECHO_GRACE * 4)),
        env,
    );
    // Busy: painting something else on every tick, for longer than
    // the grace.  This also covers the pane that never settles —
    // the quiet window is reset every tick and never reached.
    run(&mut r, &host, &state, 32);
    for tick in 1..=800u64 {
        *state.out_len.lock().unwrap() = 8 * tick;
        *state.tail.lock().unwrap() = b"spinner".to_vec();
        run(&mut r, &host, &state, 16);
        if state.sent.lock().unwrap().len() == 2 {
            break;
        }
    }
    let sent = state.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 2, "the grace is the upper bound, painting or not");
    assert_eq!(sent[1], b"\r".to_vec());
}

/// The needle survives the composer wrapping the line.
#[test]
fn the_needle_is_a_short_prefix_on_a_character_boundary() {
    // Multi-byte: cutting at 12 bytes must not split a character.
    let cn = "额度恢复了，如果有刚被打断的工作请继续";
    let n = submit_needle(cn);
    assert!(std::str::from_utf8(n).is_ok(), "the needle is not valid UTF-8");
    assert!(cn.as_bytes().starts_with(n));
    assert!(!n.is_empty() && n.len() <= 12);

    // Shorter than the window is the whole line.
    assert_eq!(submit_needle("hi"), b"hi");
}

/// A pane without bracketed paste still gets the line — there is
/// nothing to wrap it with.
#[test]
fn an_unbracketed_pane_still_gets_the_line() {
    use marspot_term::grid_shm::FLAG_ALT_SCREEN;
    let (state, env, host) = setup();
    *state.modes.lock().unwrap() = Some((1, FLAG_ALT_SCREEN));
    let mut r = OpRunner::new(PtyOp::new("test.submit").step(Step::submit("carry on")), env);
    run(&mut r, &host, &state, 32);
    assert_eq!(state.sent.lock().unwrap()[0], b"carry on".to_vec());
}

/// A pane that came back to a question is left alone.
#[test]
fn a_waiting_prompt_stops_the_script_before_it_types() {
    const NEEDLES: &[&str] = &["Enter to confirm"];
    let (state, env, host) = setup();
    *state.tail.lock().unwrap() =
        b"  Yes, I trust this folder\r\n  No, exit\r\n Enter to confirm \xc2\xb7 Esc to cancel"
            .to_vec();
    let mut r = OpRunner::new(
        PtyOp::new("test.veto")
            .step(Step::stop_if_waiting(NEEDLES))
            .step(Step::submit("carry on")),
        env,
    );
    run(&mut r, &host, &state, 200);
    assert!(
        state.sent.lock().unwrap().is_empty(),
        "the highlighted answer to the trust prompt is \"No, exit\""
    );
    assert!(matches!(r.finished, Some(OpOutcome::Done)));

    // Nothing waiting: the line goes in.
    let (state, env, host) = setup();
    *state.modes.lock().unwrap() = Some((1, 0));
    let mut r = OpRunner::new(
        PtyOp::new("test.veto")
            .step(Step::stop_if_waiting(NEEDLES))
            .step(Step::submit("carry on")),
        env,
    );
    run(&mut r, &host, &state, 200);
    assert_eq!(state.sent.lock().unwrap().len(), 1);
}

/// The reclamation script, end to end, in the world the test
/// controls: hold → settle → TERM → wait for it to die → park →
/// user comes back → type → wait for the process → wait for it to
/// finish drawing → release.
#[test]
fn a_park_and_wake_script_runs_in_order() {
    let (state, env, host) = setup();
    state.alive.lock().unwrap().push(4242);
    let op = PtyOp::new("test.park")
        .hold_screen(true)
        .badge("zZ")
        .step(Step::settle(Duration::from_millis(250)))
        .step(
            Step::terminate(fid(4242), libc::SIGTERM)
                .escalate_after(Duration::from_secs(3), libc::SIGKILL),
        )
        .step(Step::await_user())
        .step(Step::send(b"resume\r".to_vec()))
        .step(Step::await_process(1, |_| true))
        .step(Step::await_quiet(Duration::from_millis(500)));
    let mut r = OpRunner::new(op, env);

    // The hold is taken before anything else happens — the whole
    // point is that the kill never reaches the screen.
    r.on_tick(&host);
    assert_eq!(*state.holds.lock().unwrap(), vec![true]);
    assert!(state.signals.lock().unwrap().is_empty(), "settle first");
    assert!(
        !PaneSession::parked(&r),
        "a run still working is not parked — the shell wakes only \
         the ones waiting for the user"
    );

    run(&mut r, &host, &state, 300);
    assert_eq!(*state.signals.lock().unwrap(), vec![(4242, libc::SIGTERM)]);

    // Parked: it waits for the user however long that takes.
    state.alive.lock().unwrap().clear();
    run(&mut r, &host, &state, 200);
    assert!(r.is_awaiting_user(), "the script parks here");
    assert!(
        PaneSession::parked(&r),
        "…and says so from the outside, which is what the \
         come-back-to-marspot prefetch walks"
    );
    advance(&state, 3_600_000);
    run(&mut r, &host, &state, 100);
    assert!(r.is_awaiting_user(), "an hour later, still parked");
    assert!(state.sent.lock().unwrap().is_empty(), "nothing typed while parked");

    // The user comes back.
    r.on_focus(&host);
    run(&mut r, &host, &state, 100);
    assert_eq!(state.sent.lock().unwrap().len(), 1, "the line goes out");
    assert_eq!(state.sent.lock().unwrap()[0], b"resume\r");
    assert!(
        !PaneSession::parked(&r),
        "woken: a second prefetch pass must not type at it again"
    );

    // Still waiting: the process is not there yet.
    run(&mut r, &host, &state, 200);
    assert!(!*host.ended.lock().unwrap());
    state.present.lock().unwrap().push(99);
    // …and once it is, the run waits for it to draw and go still.
    run(&mut r, &host, &state, 100);
    *state.out_len.lock().unwrap() = 4096;
    run(&mut r, &host, &state, 100);
    assert!(!*host.ended.lock().unwrap(), "still drawing");
    run(&mut r, &host, &state, 600);
    assert!(*host.ended.lock().unwrap(), "quiet for long enough — done");

    r.on_end(&host, EndReason::PluginRequested);
    assert_eq!(*state.holds.lock().unwrap(), vec![true, false], "released exactly once");
}

/// A parked run must not send a badge every tick.
///
/// `on_tick` rides the redraw pump — 16 ms while anything is
/// drawing — and the step that waits for the user can hold for
/// hours.  A wire frame per pane per tick for a pane that is doing
/// nothing is the definition of background creep;
/// the hand-written version this replaced had an 8-second throttle
/// for exactly this reason, and the move to scripts lost it.
#[test]
fn a_parked_run_does_not_re_send_its_badge_every_tick() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.park").badge("zZ").step(Step::await_user());
    let mut r = OpRunner::new(op, env);
    // Ten seconds of ticks while parked.
    run(&mut r, &host, &state, 10_000);
    let sent = host.badges.lock().unwrap().len();
    assert!(
        sent <= 3,
        "10 s parked should re-assert a handful of times, sent {sent}"
    );
    assert!(sent >= 1, "and at least once, so a fresh core gets it back");
    // Parked means no spinner: there is nothing to animate.
    assert!(
        host.badges.lock().unwrap().iter().all(|b| b == "zZ"),
        "got {:?}",
        host.badges.lock().unwrap()
    );
}

/// Silence is not the same as "it has drawn".
///
/// A program that has just started emits a trickle — mode sets, a
/// cursor query, a screen clear — and then goes quiet for as long
/// as it takes to load whatever it is resuming.  With no byte
/// floor that pause reads as "finished", and the freeze lifts onto
/// the screen it just cleared: the black flash the user saw twice
/// after two different fixes, because both fixes were about *when*
/// the clock starts rather than *what* counts as drawn.
#[test]
fn a_startup_trickle_then_a_pause_is_not_a_finished_frame() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.wake").step(
        Step::await_quiet(Duration::from_millis(500)).after_bytes(2048),
    );
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 50);

    // The clear plus a few mode sets: real output, but not a frame.
    *state.out_len.lock().unwrap() = 300;
    run(&mut r, &host, &state, 2_000);
    assert!(
        !*host.ended.lock().unwrap(),
        "300 bytes and two seconds of silence is a program still loading"
    );

    // Now it actually paints.
    *state.out_len.lock().unwrap() = 300 + 40_000;
    run(&mut r, &host, &state, 200);
    assert!(!*host.ended.lock().unwrap(), "still settling");
    run(&mut r, &host, &state, 600);
    assert!(*host.ended.lock().unwrap(), "drawn, and still — now release");
}

/// Ticks the runner while the pane keeps printing a spinner: 30
/// bytes a frame, roughly what claude emits while it works.
fn run_spinning(r: &mut OpRunner, host: &Host, state: &FakeState, ms: u64) {
    for _ in 0..(ms / 16) {
        if *host.ended.lock().unwrap() {
            return;
        }
        *state.out_len.lock().unwrap() += 30;
        r.on_tick(host);
        advance(state, 16);
    }
}

const CLAUDE_BOX: &str = "⏺ resumed\n\n✻ Working…\n────────────\n❯\u{a0}\n────────────\n";

/// A resumed claude told to carry on starts working at once, and
/// its spinner never lets the pane go still. Waiting for quiet
/// held the screen frozen for the whole 30 s deadline and then
/// failed the run, skipping what came after it -- on 5 of 9 panes
/// in one profile switch.
#[test]
fn an_input_box_on_screen_ends_the_wait_for_a_pane_that_never_goes_still() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.cycle").step(
        Step::await_quiet(Duration::from_millis(1_500)).after_bytes(2048).or_composer(),
    );
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 50);

    // loading: a spinner, no box yet
    *state.screen.lock().unwrap() = "✳ Resuming conversation…".into();
    run_spinning(&mut r, &host, &state, 3_000);
    assert!(!*host.ended.lock().unwrap(), "no box is a conversation still loading");

    *state.out_len.lock().unwrap() += 40_000;
    *state.screen.lock().unwrap() = CLAUDE_BOX.into();
    run_spinning(&mut r, &host, &state, 1_000);
    assert!(!*host.ended.lock().unwrap(), "the paint is still inside the window");
    run_spinning(&mut r, &host, &state, 2_000);
    assert!(*host.ended.lock().unwrap(), "the box is up and only the spinner moves");
    assert!(matches!(r.finished, Some(OpOutcome::Done)), "{:?}", r.finished);
}

/// An agent started on a new conversation draws a banner and its
/// input box and stops, well under the floor a first frame of a
/// resumed one is measured by. The box is what says it is ready.
#[test]
fn a_new_conversations_small_first_screen_ends_the_wait_once_its_box_is_up() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.handoff").step(
        Step::await_quiet(Duration::from_millis(1_500)).after_bytes(2048).or_composer(),
    );
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 50);
    *state.out_len.lock().unwrap() += 600;
    run_spinning(&mut r, &host, &state, 3_000);
    assert!(!*host.ended.lock().unwrap(), "600 bytes and no box is startup noise");
    *state.screen.lock().unwrap() = CLAUDE_BOX.into();
    run_spinning(&mut r, &host, &state, 2_500);
    assert!(matches!(r.finished, Some(OpOutcome::Done)), "{:?}", r.finished);
}

/// A large session is drawn in passes: paint, read more of the
/// transcript, paint again. The box can be up after the first
/// pass; lifting the hold there shows the rest arriving.
#[test]
fn a_box_on_a_screen_that_is_still_repainting_does_not_end_the_wait() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.cycle").step(
        Step::await_quiet(Duration::from_millis(1_500)).after_bytes(2048).or_composer(),
    );
    let mut r = OpRunner::new(op, env);
    *state.screen.lock().unwrap() = CLAUDE_BOX.into();
    for _ in 0..6 {
        *state.out_len.lock().unwrap() += 20_000;
        run_spinning(&mut r, &host, &state, 1_000);
        assert!(!*host.ended.lock().unwrap(), "a repaint every second is not settled");
    }
    run_spinning(&mut r, &host, &state, 2_500);
    assert!(*host.ended.lock().unwrap(), "once the passes stop, it is");
}

/// The program the run was started for exits -- a resume it
/// refused. The run ends there rather than at the next deadline.
#[test]
fn a_watched_program_that_exits_ends_the_run_at_once() {
    let (state, env, host) = setup();
    state.present.lock().unwrap().push(77);
    state.alive.lock().unwrap().push(77);
    let op = PtyOp::new("test.cycle")
        .step(Step::watch_process(1, |_| true))
        .step(Step::await_quiet(Duration::from_millis(1_500)).after_bytes(2048).timeout(Duration::from_secs(30)));
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 200);
    assert!(!*host.ended.lock().unwrap(), "running, so the wait goes on");
    state.alive.lock().unwrap().clear();
    run(&mut r, &host, &state, 100);
    assert!(*host.ended.lock().unwrap());
    assert!(
        matches!(&r.finished, Some(OpOutcome::Failed { label: "await_quiet", .. })),
        "{:?}",
        r.finished
    );
}

#[test]
fn watching_a_program_that_never_started_fails_the_run() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.cycle")
        .step(Step::watch_process(1, |_| true))
        .step(Step::send(b"x".to_vec()));
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 100);
    assert!(matches!(&r.finished, Some(OpOutcome::Failed { label: "watch_process", .. })));
    assert!(state.sent.lock().unwrap().is_empty(), "nothing after it ran");
}

#[test]
fn an_input_box_does_not_count_below_the_byte_floor() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.cycle").step(
        Step::await_quiet(Duration::from_millis(1_500)).after_bytes(2048).or_composer(),
    );
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 50);
    // the screen still shows the old session's box from before the clear
    *state.screen.lock().unwrap() = CLAUDE_BOX.into();
    *state.out_len.lock().unwrap() = 300;
    for _ in 0..60 {
        r.on_tick(&host);
        advance(&state, 16);
        // a slow trickle that never reaches the floor
        if *state.out_len.lock().unwrap() < 1_500 {
            *state.out_len.lock().unwrap() += 1;
        }
    }
    assert!(!*host.ended.lock().unwrap());
}

#[test]
fn without_or_composer_a_spinning_pane_still_waits_for_quiet() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.wake")
        .step(Step::await_quiet(Duration::from_millis(1_500)).after_bytes(2048));
    let mut r = OpRunner::new(op, env);
    *state.out_len.lock().unwrap() = 40_000;
    *state.screen.lock().unwrap() = CLAUDE_BOX.into();
    run_spinning(&mut r, &host, &state, 5_000);
    assert!(!*host.ended.lock().unwrap());
}

#[test]
fn a_sent_message_is_not_an_input_box() {
    // past prompts carry the same glyph, without the rule above
    assert!(!composer_on_screen("❯ fix the bug\n⏺ done\n"));
    assert!(!composer_on_screen("✳ Resuming conversation…"));
    assert!(composer_on_screen(CLAUDE_BOX));
    assert!(composer_on_screen("│ ─────── │\n│ ❯ half a sentence │\n"));
}

/// A run that exists to bring something back must not do it twice.
///
/// The gap between parking a session and typing it back holds the
/// user's own click, and in that gap the session can return by
/// other means — a second wake armed on the same pane, or the user
/// starting it themselves.  Typing then puts `claude --resume …`
/// into the running session's prompt, as text they have to delete.
/// Seen on the real machine twice in one day, which is what this
/// step is for.
#[test]
fn a_run_stops_when_what_it_was_arranging_has_already_happened() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.wake")
        .step(Step::await_user())
        .step(Step::stop_if_process(1, |_| true))
        .step(Step::send(b"resume\r".to_vec()));
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 100);
    assert!(r.is_awaiting_user());

    // It came back on its own while we were parked.
    state.present.lock().unwrap().push(99);
    r.on_focus(&host);
    run(&mut r, &host, &state, 100);
    assert!(
        state.sent.lock().unwrap().is_empty(),
        "nothing to type — it is already back"
    );
    assert!(*host.ended.lock().unwrap(), "and the run is over, successfully");
}

/// …and when it has not, the run carries on as before.
#[test]
fn a_run_carries_on_when_nothing_has_come_back() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.wake")
        .step(Step::await_user())
        .step(Step::stop_if_process(1, |_| true))
        .step(Step::send(b"resume\r".to_vec()));
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 100);
    r.on_focus(&host);
    run(&mut r, &host, &state, 100);
    assert_eq!(*state.sent.lock().unwrap(), vec![b"resume\r".to_vec()]);
}

/// A signal that doesn't take gets escalated, and the wait that
/// follows is the one that notices.
#[test]
fn a_stubborn_process_gets_escalated() {
    let (state, env, host) = setup();
    state.alive.lock().unwrap().push(7);
    let op = PtyOp::new("test.kill").step(
        Step::terminate(fid(7), libc::SIGTERM)
            .escalate_after(Duration::from_secs(3), libc::SIGKILL)
            .timeout(Duration::from_secs(10)),
    );
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 1_000);
    assert_eq!(*state.signals.lock().unwrap(), vec![(7, libc::SIGTERM)], "polite first");
    run(&mut r, &host, &state, 3_000);
    assert!(
        state.signals.lock().unwrap().contains(&(7, libc::SIGKILL)),
        "then the deadline"
    );
    assert!(*host.ended.lock().unwrap(), "and the wait completes once it is gone");
}

/// A target that turns out to be gone when it is signalled is the
/// step done, not the run failed: the process the decision was
/// about has already left, which is what the step was waiting for.
#[test]
fn a_target_already_gone_ends_the_step_and_the_run_goes_on() {
    let (state, env, host) = setup();
    state.alive.lock().unwrap().push(7);
    state.refuse.lock().unwrap().push((libc::SIGTERM, SignalError::Gone));
    let finished: Arc<Mutex<Option<OpOutcome>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&finished);
    let op = PtyOp::new("test.gone")
        .step(Step::terminate(fid(7), libc::SIGTERM).timeout(Duration::from_secs(10)))
        .step(Step::send(b"after\r".to_vec()));
    let mut r = OpRunner::new(op, env).on_finish(move |_, o| *sink.lock().unwrap() = Some(o.clone()));
    run(&mut r, &host, &state, 200);
    assert_eq!(*state.signals.lock().unwrap(), vec![(7, libc::SIGTERM)], "asked once");
    assert_eq!(*state.sent.lock().unwrap(), vec![b"after\r".to_vec()], "the next step ran");
    assert_eq!(*state.mouse_resets.lock().unwrap(), vec![1], "what it left on is ours to clear");
    assert!(*host.ended.lock().unwrap(), "the run asked to end");
    r.on_end(&host, EndReason::PluginRequested);
    assert_eq!(*finished.lock().unwrap(), Some(OpOutcome::Done));
}

/// One we may not signal fails the run at that step: nothing after
/// it may act as though the process were gone.
#[test]
fn a_target_we_may_not_signal_fails_the_run() {
    let (state, env, host) = setup();
    state.alive.lock().unwrap().push(7);
    state.refuse.lock().unwrap().push((libc::SIGTERM, SignalError::Denied));
    let finished: Arc<Mutex<Option<OpOutcome>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&finished);
    let op = PtyOp::new("test.denied")
        .step(Step::terminate(fid(7), libc::SIGTERM).timeout(Duration::from_secs(10)))
        .step(Step::send(b"after\r".to_vec()));
    let mut r = OpRunner::new(op, env).on_finish(move |_, o| *sink.lock().unwrap() = Some(o.clone()));
    run(&mut r, &host, &state, 200);
    assert!(*host.ended.lock().unwrap(), "the run asked to end");
    r.on_end(&host, EndReason::PluginRequested);
    match finished.lock().unwrap().clone() {
        Some(OpOutcome::Failed { step: 0, label: "terminate", err }) => {
            assert!(err.contains("pid 7"), "the error names the process: {err}")
        }
        other => panic!("a refused signal fails the terminate step, got {other:?}"),
    }
    assert!(state.sent.lock().unwrap().is_empty(), "nothing after the step ran");
    assert!(state.mouse_resets.lock().unwrap().is_empty(), "the program is still there");
}

/// The escalation is checked the same way: a process that went in
/// the seconds between the polite signal and the deadline is done.
#[test]
fn a_target_gone_by_the_escalation_ends_the_step() {
    let (state, env, host) = setup();
    state.alive.lock().unwrap().push(7);
    state.refuse.lock().unwrap().push((libc::SIGKILL, SignalError::Gone));
    let finished: Arc<Mutex<Option<OpOutcome>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&finished);
    let op = PtyOp::new("test.late").step(
        Step::terminate(fid(7), libc::SIGTERM)
            .escalate_after(Duration::from_secs(3), libc::SIGKILL)
            .timeout(Duration::from_secs(10)),
    );
    let mut r = OpRunner::new(op, env).on_finish(move |_, o| *sink.lock().unwrap() = Some(o.clone()));
    run(&mut r, &host, &state, 1_000);
    assert_eq!(finished.lock().unwrap().clone(), None, "still waiting on the polite one");
    run(&mut r, &host, &state, 3_000);
    assert_eq!(
        *state.signals.lock().unwrap(),
        vec![(7, libc::SIGTERM), (7, libc::SIGKILL)]
    );
    assert!(*host.ended.lock().unwrap(), "the run asked to end");
    r.on_end(&host, EndReason::PluginRequested);
    assert_eq!(*finished.lock().unwrap(), Some(OpOutcome::Done));
}

/// Killing the foreground program is also giving up the modes it
/// owned.  Nothing sends `CSI ? 1002 l` on behalf of a process that
/// took a signal, so unless the terminal is told, the shell that
/// surfaces underneath inherits mouse reporting — and the next
/// scroll is typed into its prompt as `CSI < 64;x;y M`
/// (2026-09-01).
#[test]
fn a_terminated_program_gives_up_its_mouse_reporting() {
    let (state, env, host) = setup();
    state.alive.lock().unwrap().push(7);
    let op = PtyOp::new("test.kill").step(
        Step::terminate(fid(7), libc::SIGTERM).timeout(Duration::from_secs(10)),
    );
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 100);
    assert_eq!(*state.signals.lock().unwrap(), vec![(7, libc::SIGTERM)]);
    assert!(
        state.mouse_resets.lock().unwrap().is_empty(),
        "not while it is still running"
    );
    // It takes the signal and goes — what claude does, in ~800 ms.
    state.alive.lock().unwrap().retain(|p| *p != 7);
    run(&mut r, &host, &state, 100);
    assert_eq!(
        *state.mouse_resets.lock().unwrap(),
        vec![1],
        "the pane whose program we killed is told, exactly once"
    );
}

/// The reset rides on the process being *gone*, not on the signal
/// being sent — a program that ignores SIGTERM still owns its
/// modes, and the escalation is what settles it.
#[test]
fn mouse_reporting_survives_until_the_process_actually_dies() {
    let (state, env, host) = setup();
    state.alive.lock().unwrap().push(7);
    let op = PtyOp::new("test.kill").step(
        Step::terminate(fid(7), libc::SIGTERM)
            .escalate_after(Duration::from_secs(3), libc::SIGKILL)
            .timeout(Duration::from_secs(10)),
    );
    let mut r = OpRunner::new(op, env);
    run(&mut r, &host, &state, 1_000);
    assert!(
        state.mouse_resets.lock().unwrap().is_empty(),
        "still alive, still its terminal"
    );
    run(&mut r, &host, &state, 3_000);
    assert_eq!(*state.mouse_resets.lock().unwrap(), vec![1], "gone now");
}

/// Every wait has a deadline, and blowing it is reported rather
/// than hung on.
#[test]
fn a_step_that_never_completes_times_out() {
    let (state, env, host) = setup();
    state.alive.lock().unwrap().push(7);
    let finished: Arc<Mutex<Option<OpOutcome>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&finished);
    let op = PtyOp::new("test.stuck").hold_screen(true).step(
        Step::terminate(fid(7), libc::SIGTERM).timeout(Duration::from_secs(2)),
    );
    let mut r = OpRunner::new(op, env)
        .on_finish(move |_, o| *sink.lock().unwrap() = Some(o.clone()));
    run(&mut r, &host, &state, 5_000);
    r.on_end(&host, EndReason::PluginRequested);
    assert_eq!(
        *finished.lock().unwrap(),
        Some(OpOutcome::TimedOut { step: 0, label: "terminate" })
    );
    assert_eq!(
        *state.holds.lock().unwrap(),
        vec![true, false],
        "a timed-out op still releases the pane"
    );
}

/// Esc gets the pane back, and the cleanup still runs.
#[test]
fn escape_ends_the_run_and_releases_the_pane() {
    let (state, env, host) = setup();
    let op = PtyOp::new("test.esc")
        .hold_screen(true)
        .step(Step::settle(Duration::from_secs(30)));
    let mut r = OpRunner::new(op, env);
    r.on_tick(&host);
    let esc = marspot::shell_proto::WireKeyEvent {
        state: marspot::shell_proto::WireKeyState::Pressed,
        mods: 0,
        kind: marspot::shell_proto::WireLogicalKind::Named,
        key_data: marspot::shell_proto::WireNamedKey::Escape as u32,
        text: Default::default(),
        ..Default::default()
    };
    assert!(matches!(r.on_user_key(&host, &esc), KeyHandling::EndSession));
    r.on_end(&host, EndReason::UserEscape);
    assert_eq!(*state.holds.lock().unwrap(), vec![true, false]);
}

/// The line is run by a shell, so a test that only reads it is
/// testing a string. This one hands it to `/bin/sh` and checks
/// what came out.
///
/// It exists because the first version put `exec` before the
/// environment assignments, which asks the shell to run a program
/// called `CLAUDE_CONFIG_DIR=/…`: every switch exited 127 and then
/// spent twenty seconds waiting for a process that was never
/// started. Every assertion about the text passed.
#[test]
fn the_line_a_shell_is_given_actually_runs() {
    let dir = std::env::temp_dir();
    let line = PtyCommand::new("/bin/sh")
        .env("MARSPOT_PROBE", "ran")
        .in_dir(Some(dir.to_string_lossy().to_string()))
        .arg("-c")
        .quoted_arg("printf %s-%s \"$MARSPOT_PROBE\" \"$(basename \"$PWD\")\"")
        .to_bytes()
        .expect("a line");
    let text = String::from_utf8(line).unwrap();
    let text = text.trim_end_matches('\r');
    let out = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(text)
        .output()
        .expect("run it");
    assert!(
        out.status.success(),
        "the line a pane would be typed exits {:?}: {text}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    let want_dir = dir
        .canonicalize()
        .unwrap_or(dir.clone())
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    assert_eq!(
        said,
        format!("ran-{want_dir}"),
        "the environment reached the program and so did the directory"
    );
}

/// A value that could break out of its quoting is refused, not
/// escaped — the caller gets nothing rather than a line that does
/// something else.
#[test]
fn a_command_refuses_values_it_cannot_quote() {
    let ok = PtyCommand::new("claude")
        .env("CLAUDE_CONFIG_DIR", "/Users/x/.claude-profile-3")
        .arg("--resume")
        .arg("abc-123")
        .clear_screen_first(true)
        .to_bytes()
        .expect("safe parts build a line");
    assert_eq!(
        String::from_utf8(ok).unwrap(),
        "printf '\\033[H\\033[2J'; CLAUDE_CONFIG_DIR='/Users/x/.claude-profile-3' \
         claude --resume abc-123\r"
    );
    for bad in ["/tmp/a'; rm -rf ~; '", "/tmp/$(whoami)", "/tmp/`id`", "", "a\nb"] {
        assert!(
            PtyCommand::new("claude").env("D", bad).to_bytes().is_none(),
            "{bad:?} must be refused"
        );
    }
    // The wipe never takes the scrollback with it.
    let line = PtyCommand::new("claude").clear_screen_first(true).to_bytes().unwrap();
    assert!(!String::from_utf8(line).unwrap().contains("[3J"));
}

/// Text submitted into a bracketed-paste pane cannot close the paste
/// early: the only end marker written is the one that closes it.
#[test]
fn submit_paste_drops_end_marker_in_the_text() {
    use marspot_term::grid_shm::{FLAG_ALT_SCREEN, FLAG_BRACKETED_PASTE};
    let (state, env, host) = setup();
    *state.modes.lock().unwrap() = Some((1, FLAG_ALT_SCREEN | FLAG_BRACKETED_PASTE));
    let mut r = OpRunner::new(
        PtyOp::new("test.submit").step(Step::submit("a\x1b[201~b\x1b[20\x1b[201~1~c")),
        env,
    );
    run(&mut r, &host, &state, 32);
    assert_eq!(
        state.sent.lock().unwrap().clone(),
        vec![b"\x1b[200~abc\x1b[201~".to_vec()],
    );
}

#[path = "origin_tests.rs"]
mod origin_tests;

#[path = "badge_tests.rs"]
mod badge_tests;
