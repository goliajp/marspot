//! The cross-agent switch run end to end on the fake pane: a first
//! frame that is still drawing at its deadline fails the switch before
//! the ledger is committed, so the turns not yet handed over are
//! handed over again by the next switch.

use super::*;
use crate::plugins::handoff::{switch_op, Agent, Leaving};
use marspot_term::test_env::{EnvGuard, TempDir};

const SID: u64 = 1;
const SHELL: i32 = 4200;

/// Ticks the switch, feeding the pane what each step waits for: a
/// prompt after the interrupt, then a first frame that either keeps
/// repainting (`late`) or paints and stops.
fn drive(r: &mut OpRunner, host: &Host, state: &FakeState, late: bool) {
    let mut painted = 0u64;
    for _ in 0..(90_000 / 16) {
        if *host.ended.lock().unwrap() {
            return;
        }
        let label = r.op.steps.get(r.at).map_or("", |s| s.label);
        match label {
            // the job thread reads the history in real time
            "write_handoff" => std::thread::sleep(Duration::from_millis(1)),
            "fresh_prompt" if painted == 0 => {
                *state.out_len.lock().unwrap() += 40;
                painted = 1;
            }
            "first_frame" if late || painted < 40_000 => {
                *state.out_len.lock().unwrap() += 400;
                painted += 400;
            }
            _ => {}
        }
        r.on_tick(host);
        advance(state, 16);
    }
}

fn switch_to_claude(late: bool) -> (Option<OpOutcome>, bool, Vec<String>) {
    let root = TempDir::new("handoff-late");
    let mut env = EnvGuard::new();
    env.set("MARSPOT_STATE_DIR", root.path()).set("MARSPOT_PEER_BIN", "/a/marspot-peer");
    let leaving = Leaving {
        agent: Agent::Codex,
        proc: ProcId { pid: 4242, start_us: 0 },
        shell_pid: SHELL,
        home: root.join("codex-home"),
        session_id: None,
        cwd: None,
        go_on: false,
    };
    let op = switch_op(SID, leaving, Agent::Claude, 2, root.join("claude-home")).expect("switch op");
    let (state, fake, host) = setup();
    state.present.lock().unwrap().push(4300);
    let mut r = OpRunner::new(op, fake);
    drive(&mut r, &host, &state, late);
    let committed = root.join("handoff").join(format!("{SID}.ledger")).exists();
    let logs = host.logs.lock().unwrap().clone();
    (r.finished.clone(), committed, logs)
}

#[test]
fn a_late_first_frame_fails_the_switch_and_leaves_the_ledger_uncommitted() {
    let (outcome, committed, logs) = switch_to_claude(true);
    assert!(matches!(outcome, Some(OpOutcome::TimedOut { label: "first_frame", .. })), "{outcome:?} {logs:?}");
    assert!(!committed, "a switch that did not hand over its turns does not mark them handed over");
}

/// The same switch with a first frame that stops drawing commits it,
/// so the check above can fail.
#[test]
fn a_first_frame_that_stops_drawing_commits_the_ledger() {
    let (outcome, committed, logs) = switch_to_claude(false);
    assert_eq!(outcome, Some(OpOutcome::Done), "{logs:?}");
    assert!(committed, "{logs:?}");
}
