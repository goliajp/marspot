//! `PaneWatch`: each kind fires when it should and not before, the
//! table's limits, and reports that must not be lost.

use std::time::{Duration, Instant};

use marspot_term::shell_proto::{DoneCode, TypedResult, WatchBody, WatchKind, WatchReason};
use marspot_term::test_env::sandbox_state_dir;

use crate::pane_link::{Link, body, text, with_zsh};

const NEVER: u64 = u64::MAX - 1;
const HELPER: u32 = 1 << 31;

/// The OSC 133 command end a shell's prompt hook prints, with `status`.
fn end_mark(status: &str) -> String {
    format!("printf '\\033]133;D;%s\\007' {status}\r")
}

#[test]
fn watch_kinds_fire_when_true_and_not_before() {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);

    let p = l.pos();
    l.armed(body(1, WatchKind::Change, p));
    assert!(l.try_fired(1, 0.3).is_none(), "change: no output yet");
    l.inject("x");
    let f = l.fired(1, 5.0);
    assert!(f.pos > p && f.reason == WatchReason::Met, "{f:?}");
    l.inject("\u{15}");

    let p = l.pos();
    l.armed(body(2, WatchKind::CommandEnd, p));
    assert!(l.try_fired(2, 0.3).is_none(), "command end: none since");
    l.inject(&end_mark("7"));
    assert_eq!(l.fired(2, 5.0).status, Some(7));

    std::thread::sleep(Duration::from_millis(300));
    let p = l.pos();
    l.armed(WatchBody { quiet_ms: 300, ..body(3, WatchKind::Quiet, p) });
    assert!(l.try_fired(3, 0.6).is_none(), "quiet: no output since, so nothing to be quiet after");
    l.inject("echo hi\r");
    let started = Instant::now();
    l.fired(3, 5.0);
    assert!(started.elapsed() >= Duration::from_millis(280), "quiet fired {:?} after the output", started.elapsed());

    let p = l.pos();
    l.armed(body(4, WatchKind::Prompt, p));
    l.inject("sleep 1\r");
    assert!(l.try_fired(4, 0.8).is_none(), "prompt: a command has the terminal");
    let f = l.fired(4, 5.0);
    assert!(f.pos > p);

    let p = l.pos();
    l.armed(text(5, p, "fired-text"));
    l.inject("printf 'fired-%s\\n' nothing\r");
    assert!(l.try_fired(5, 0.5).is_none(), "text: not printed");
    l.inject("printf 'fired-%s\\n' text\r");
    assert!(l.fired(5, 5.0).line.is_some());

    l.armed(body(6, WatchKind::Exit, 0));
    l.armed(body(7, WatchKind::Change, NEVER));
    assert!(l.try_fired(6, 0.3).is_none(), "exit: the shell runs");
    l.inject("exit\r");
    assert_eq!(l.fired(6, 5.0).reason, WatchReason::Met, "exit is what an exit watch waits for");
    assert_eq!(l.fired(7, 5.0).reason, WatchReason::SessionEnded);
}

#[test]
fn watch_already_true_at_registration_fires_at_once() {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    let p = l.pos();
    l.inject(&end_mark("3"));
    crate::pane_link::wait(5.0, "the command end", || l.pos() > p + 20);
    std::thread::sleep(Duration::from_millis(200));
    l.armed(body(1, WatchKind::CommandEnd, p));
    let f = l.fired(1, 1.0);
    assert_eq!(f.status, Some(3));
    assert!(f.pos > p);
    l.armed(body(2, WatchKind::Change, 0));
    l.fired(2, 1.0);
}

#[test]
fn watch_same_id_twice_replaces_and_the_table_holds_seven_and_one() {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    l.armed(body(1, WatchKind::Change, NEVER));
    l.armed(body(1, WatchKind::Change, 0));
    assert_eq!(l.fired(1, 1.0).watch_id, 1, "the second registration replaced the first");
    for id in 2..=7 {
        l.armed(body(id, WatchKind::Change, NEVER));
    }
    let req = l.watch(body(8, WatchKind::Change, NEVER));
    assert_eq!(l.done(req).code, DoneCode::WatchTableFull, "seven for callers");
    l.armed(body(HELPER | 1, WatchKind::Change, NEVER));
    let req = l.watch(body(HELPER | 2, WatchKind::Change, NEVER));
    assert_eq!(l.done(req).code, DoneCode::WatchTableFull, "one for helpers");
    let req = l.watch(text(9, 0, &"t".repeat(257)));
    let d = l.done(req);
    assert_eq!(d.code, DoneCode::Other);
    assert!(d.text.contains("256"), "{d:?}");
}

/// A watch typed with its input holds its slot from the start: a
/// watch registered while the input waits cannot take it, and the
/// request is answered once.
#[test]
fn watch_typed_with_its_input_holds_its_slot_while_the_input_waits() {
    let mut sandbox = sandbox_state_dir();
    let s = crate::pane_link::with_script(&mut sandbox, "stty raw -echo; printf READY; sleep 1; exec cat > /dev/null");
    let mut l = Link::new(&s);
    // more than a raw tty takes before its program reads
    let req = l.type_in(&[b'a'; 4096], false, false, false, Some(body(1, WatchKind::Change, NEVER)));
    for id in 2..=7 {
        l.armed(body(id, WatchKind::Change, NEVER));
    }
    let late = l.watch(body(8, WatchKind::Change, NEVER));
    assert_eq!(l.done(late).code, DoneCode::WatchTableFull, "the typed watch's slot is taken");
    assert_eq!(l.typed(req, 5.0).result, TypedResult::Written);
    let refused = l.try_take(0.3, |r| match r {
        crate::pane_link::Reply::Done(d) if d.req_id == req => Some(d.clone()),
        _ => None,
    });
    assert!(refused.is_none(), "written and refused both: {refused:?}");
    l.cancel(1);
    l.armed(body(8, WatchKind::Change, NEVER));
}

/// A shell hook's own command end right behind the one a command
/// printed, in one write: the watch reports the command's.
#[test]
fn watch_command_end_reports_the_first_end_after_its_position() {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    let p = l.pos();
    l.armed(body(1, WatchKind::CommandEnd, p));
    l.inject("printf '\\033]133;D;1\\007\\033]133;D;0\\007'\r");
    assert_eq!(l.fired(1, 5.0).status, Some(1));
}

#[test]
fn watch_clear_all_empties_the_table() {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    for id in 1..=7 {
        l.armed(body(id, WatchKind::Change, NEVER));
    }
    l.cancel(0);
    for id in 11..=17 {
        l.armed(body(id, WatchKind::Change, 0));
        l.fired(id, 1.0);
    }
    l.inject("x");
    assert!(l.try_fired(1, 0.3).is_none(), "a cleared watch says nothing");
}

#[test]
fn watch_report_refused_by_the_writer_is_sent_on_the_next_turn() {
    let mut sandbox = sandbox_state_dir();
    sandbox.env.set("MARSPOT_TEST_REFUSE_WATCH_REPORTS", "2");
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    l.armed(body(1, WatchKind::Change, 0));
    let f = l.fired(1, 2.0);
    assert_eq!(f.reason, WatchReason::Met);
    assert!(l.try_fired(1, 0.3).is_none(), "sent once it went out, not again");
}

/// A command and the command end its shell hook would print, typed with
/// a return and a command-end watch in one request.
fn run(l: &mut Link, command: &str) -> (Duration, Option<i16>) {
    let input = format!("{command}\nprintf '\\033]133;D;%s\\007' $?");
    let started = Instant::now();
    let req = l.type_in(input.as_bytes(), true, true, true, Some(body(1, WatchKind::CommandEnd, 0)));
    let f = l.fired(1, 5.0);
    let took = started.elapsed();
    assert_eq!(l.typed(req, 1.0).result, TypedResult::Written);
    l.cancel(1);
    (took, f.status)
}

#[test]
fn watch_command_end_typed_with_its_command_answers_true_every_time() {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    for i in 0..200 {
        let (took, status) = run(&mut l, "true");
        assert_eq!(status, Some(0), "run {i}");
        assert!(took < Duration::from_secs(1), "run {i} took {took:?}");
    }
    assert_eq!(run(&mut l, "false").1, Some(1));
}
