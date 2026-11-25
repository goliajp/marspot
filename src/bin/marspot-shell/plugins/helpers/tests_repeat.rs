//! What a parent is told when a notice about another helper opens its
//! helper's next turn: from claude's records through the tick, the
//! worker and a real inbox.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use marspot::pane_state::{BusyKind, PaneStatus};
use marspot_peer::control::AgentKind;
use marspot_term::test_env::{StateSandbox, sandbox_state_dir};

use super::notify::{self, Stop};
use super::table::{Event, Helper};
use super::tests::{feed, host_with, shared_at, started};
use super::{Shared, tick, turn, work};
use crate::plugins::claudecode::with_helpers::notice_turn::tests::{NOTICE, TOOL, TOOL_RESULT, Turn};
use crate::plugins::claudecode::with_helpers::turns;

/// A claude helper (1) whose parent (10) has an inbox here.
struct Told {
    _sandbox: StateSandbox,
    s: std::sync::Arc<Shared>,
    inbox: Receiver<String>,
    in_turn: HashMap<u64, turn::Turn>,
    transcript: Vec<String>,
    ended: u32,
    notices: usize,
}

impl Told {
    fn new() -> Self {
        let mut sandbox = sandbox_state_dir();
        let dir = sandbox.path().to_path_buf();
        sandbox.env.set("HOME", &dir).remove("CLAUDE_CONFIG_DIR");
        let sessions = dir.join(".claude").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let socket = dir.join("i.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (tx, inbox) = channel();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                for line in BufReader::new(conn).lines().map_while(Result::ok) {
                    let _ = tx.send(line);
                }
            }
        });
        let me = std::process::id() as i32;
        let start = marspot_peer::proc::lstart(me).unwrap();
        let record = format!(r#"{{"pid":{me},"sessionId":"s","name":"10","procStart":"{start}","messagingSocketPath":"{}"}}"#, socket.display());
        std::fs::write(sessions.join(format!("{me}.json")), record).unwrap();
        let (s, _rx) = shared_at(&dir.join("helpers"));
        feed(&s, 1, vec![Event::started(started(10, "claude", AgentKind::Claude)), Event::TaskDelivered]);
        std::fs::create_dir_all(s.dir_of(1)).unwrap();
        work::start(std::sync::Arc::clone(&s)).unwrap();
        Told { _sandbox: sandbox, s, inbox, in_turn: HashMap::new(), transcript: Vec::new(), ended: 0, notices: 0 }
    }

    fn result(&self, text: &str) {
        std::fs::write(self.s.dir_of(1).join("result.md"), text).unwrap();
        feed(&self.s, 1, vec![Event::ResultChanged]);
    }

    /// Claude takes `t`; the ticks after the scan that reads its end.
    fn takes(&mut self, t: Turn) {
        self.ended += 1;
        self.transcript.extend(t.ends(&format!("2099-01-01T00:00:{:02}.000Z", self.ended)));
        let look = turns::look_in("record", Some(&self.transcript.join("\n")));
        assert!(matches!(look, Some(turns::Look::Over { .. })), "{look:?}");
        turns::publish(look.map(|l| (1, l)).into_iter().collect());
        for _ in 0..3 {
            let host = host_with(&[(1, PaneStatus::Busy(BusyKind::Output))]);
            tick::tick(&self.s, &host, &mut self.in_turn, &mut tick::Parked::default());
        }
    }

    /// The notice that came first since the last one, once its facts
    /// are written.
    fn told(&mut self) -> String {
        let line = self.inbox.recv_timeout(Duration::from_secs(30)).expect("a notice");
        self.notices += 1;
        let end = Instant::now() + Duration::from_secs(10);
        while self.s.table().map[&1].notices.len() < self.notices || self.s.table().map[&1].result_new {
            assert!(Instant::now() < end, "its facts");
            std::thread::sleep(Duration::from_millis(5));
        }
        line
    }
}

#[test]
fn helpers_a_result_is_not_told_again_for_a_notice_about_another_helper() {
    let mut h = Told::new();
    h.result("THE-RESULT");
    h.takes(Turn::by_text("the task").with(TOOL).with(TOOL_RESULT).says("done"));
    let first = h.told();
    assert!(first.contains("THE-RESULT") && !first.contains("unchanged"), "{first}");

    // another helper under it, which it did not start, stops; a second
    // one still runs
    let child = || vec![Event::started(started(1, "shell", AgentKind::Codex)), Event::TaskDelivered];
    feed(&h.s, 2, [child(), vec![Event::IdleNoticed]].concat());
    feed(&h.s, 3, child());
    h.takes(Turn::by_marspot(NOTICE).says("Not a helper of mine; nothing to do."));
    h.takes(Turn::by_marspot(NOTICE).says("Nor is that one."));
    assert_eq!(h.in_turn[&1], turn::Turn::Idle);
    feed(&h.s, 2, vec![Event::Stopped { exited: false }]);
    feed(&h.s, 3, vec![Event::Stopped { exited: false }]);
    *h.s.lock().child_gone.get_mut(&1).unwrap() -= turn::AFTER_HELPERS_MS;

    // it works: told, and by the order nothing was before
    h.takes(Turn::by_marspot(NOTICE).with(TOOL).with(TOOL_RESULT).says("Ran it.").says("marker"));
    let again = h.told();
    assert!(again.contains("Its result is unchanged since the last notice") && again.contains("helpers/1/result.md"), "{again}");
    assert!(!again.contains("THE-RESULT") && !again.contains("more time(s)"), "{again}");
    assert_eq!(h.s.table().map[&1].results_delivered, 1);

    // a new result, with no tool seen
    h.result("ANOTHER");
    h.takes(Turn::by_marspot(NOTICE).says("Wrote it."));
    assert!(h.told().contains("ANOTHER"));
    assert!(h.inbox.recv_timeout(Duration::from_millis(300)).is_err());
    h.s.park();
}

#[test]
fn helpers_a_notice_turn_that_asks_is_told_and_so_is_a_first_stop() {
    let mut h = Told::new();
    // nothing was told yet: what it says may be all there is
    h.takes(Turn::by_text("the task").says("Waiting."));
    let first = h.told();
    assert!(first.contains("has not written its result file"), "{first}");
    h.takes(Turn::by_marspot(NOTICE).says("Which one shall I keep?"));
    assert!(h.told().contains("waits for its next input"));
    h.s.park();
}

#[test]
fn helpers_the_turn_after_a_held_end_is_told_whatever_it_did() {
    let mut h = Told::new();
    h.result("ONE");
    h.takes(Turn::by_text("the task").says("done"));
    h.told();
    // told; then it hands more work on, and its end waits for that
    feed(&h.s, 2, vec![Event::started(started(1, "claude", AgentKind::Codex)), Event::TaskDelivered]);
    h.takes(Turn::by_text("and this").with(TOOL).with(TOOL_RESULT).says("Started one."));
    assert_eq!(h.in_turn[&1], turn::Turn::Held);
    feed(&h.s, 2, vec![Event::Stopped { exited: false }]);
    *h.s.lock().child_gone.get_mut(&1).unwrap() -= turn::AFTER_HELPERS_MS;
    h.takes(Turn::by_marspot(NOTICE).says("It is 391."));
    assert!(h.told().contains("unchanged"));
    h.s.park();
}

#[test]
fn helpers_a_notice_compares_the_result_it_carries_with_the_last() {
    let mut h = Helper::new(1, started(10, "shell", AgentKind::Codex), 500);
    let text = |h: &Helper, r: &str| notify::text("a#1", h, Stop::Idle, std::path::Path::new("/r/result.md"), Some(r), None, None);
    assert!(text(&h, "x").contains("<helper-result"), "none carried");
    h.result_sha = notify::sha("x");
    assert!(text(&h, "x").contains("unchanged since the last notice; it is in /r/result.md.") && !text(&h, "x").contains("<helper-result"));
    assert!(text(&h, "y").contains("<helper-result"));
    for stop in [Stop::Exited, Stop::Budget] {
        let last = notify::text("a#1", &h, stop, std::path::Path::new("/r/result.md"), Some("x"), None, None);
        assert!(last.contains("<helper-result") && !last.contains("unchanged"), "{last}");
    }
}
