//! Cmd-Up and Cmd-Down on a real session: the pane asks, the session
//! moves its view, and the pane follows it with the prompt's line on
//! its top row, marked for a moment.

use std::time::{Duration, Instant};

use super::*;
use marspot::shell_proto::{decode_prompt_jumped, PromptDir};
use marspot_term::session_registry as reg;

const RECORDING: &[u8] = b"clear; for n in 1 2 3; do printf '\\033]133;A\\007$ cmd%s\\n' $n; \
    for i in $(seq 1 30); do echo out$n.$i; done; done; echo do''ne.\r";

fn top_row(pane: &Pane) -> String {
    pane.screen_rows().into_iter().next().map(|r| r.text.trim_end().to_string()).unwrap_or_default()
}

/// Hand the session's answers to the pane as the core's loop does, and
/// read its newest frame.  Counts the jump answers in `answers`.
fn pump(pane: &mut Pane, rx: &Receiver<CoreEvent>, answers: &mut usize) {
    while let Ok(ev) = rx.try_recv() {
        if let CoreEvent::L3Says(_, frame) = ev {
            if frame.msg_type == MsgType::PromptJumped {
                *answers += 1;
                pane.prompt_jumped(decode_prompt_jumped(&frame.payload).unwrap(), Instant::now());
            } else {
                pane.session_mut().l3_session_says(&frame);
            }
        }
    }
    pane.pump();
}

fn wait(pane: &mut Pane, rx: &Receiver<CoreEvent>, what: &str, done: impl Fn(&Pane) -> bool) -> usize {
    let mut answers = 0;
    wait_counting(pane, rx, what, &mut answers, |p, _| done(p));
    answers
}

fn wait_counting(
    pane: &mut Pane,
    rx: &Receiver<CoreEvent>,
    what: &str,
    answers: &mut usize,
    done: impl Fn(&Pane, usize) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        pump(pane, rx, answers);
        if done(pane, *answers) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("{what}; the top row is {:?} at offset {}", top_row(pane), pane.view_offset());
}

#[test]
fn cmd_up_and_down_put_the_prompts_on_the_top_row_of_the_pane() {
    let _sb = Sandbox::new();
    let (tx, rx) = marspot_term::mailbox::channel();
    let sid = reg::allocate_next_session_id().unwrap();
    let mut pane = spawn_l3_pane_with_cwd(60, 16, sid, "", &tx).expect("real L3 spawn (is marspot-session built?)");
    pane.session_mut().forward_inject_input(RECORDING);
    wait(&mut pane, &rx, "the recording never finished", |p| p.screen_rows().iter().any(|r| r.text.trim_end() == "done."));

    let mut offsets = Vec::new();
    for (dir, want) in [
        (PromptDir::Back, "$ cmd3"),
        (PromptDir::Back, "$ cmd2"),
        (PromptDir::Forward, "$ cmd3"),
    ] {
        let before = pane.view_offset();
        pane.jump_to_prompt(dir);
        wait(&mut pane, &rx, &format!("no jump to {want}"), |p| p.view_offset() != before && top_row(p) == want);
        assert!(pane.prompt_flash_until().is_some(), "the row the jump put on top is not marked");
        offsets.push(pane.view_offset());
    }
    assert_eq!(offsets[0], offsets[2], "back and forth again is not where it started");
    assert!(offsets[1] > offsets[0]);
    std::mem::forget(pane);
}

/// Cmd-Up and a key straight after it, before the answer is back: the
/// session jumps and then goes live for the key, so the late answer is
/// dropped and the pane stays live, and a scroll to where the jump went
/// is still sent and shown.
#[test]
fn a_key_after_a_jump_wins_over_the_jumps_late_answer() {
    let _sb = Sandbox::new();
    let (tx, rx) = marspot_term::mailbox::channel();
    let sid = reg::allocate_next_session_id().unwrap();
    let mut pane = spawn_l3_pane_with_cwd(60, 16, sid, "", &tx).expect("real L3 spawn (is marspot-session built?)");
    pane.session_mut().forward_inject_input(RECORDING);
    wait(&mut pane, &rx, "the recording never finished", |p| p.screen_rows().iter().any(|r| r.text.trim_end() == "done."));

    pane.jump_to_prompt(PromptDir::Back);
    wait(&mut pane, &rx, "no jump to $ cmd3", |p| p.view_offset() != 0 && top_row(p) == "$ cmd3");
    let third = pane.view_offset();
    assert!(pane.snap_to_live());
    wait(&mut pane, &rx, "not live again", |p| top_row(p) != "$ cmd3");

    pane.jump_to_prompt(PromptDir::Back);
    let space = MarspotKeyEvent {
        state: marspot::input::KeyState::Pressed,
        logical: marspot::input::LogicalKey::Char(' '),
        text: Some(" ".into()),
        ..Default::default()
    };
    pane.forward_key(&space, Modifiers::default());
    let mut answers = 0;
    wait_counting(&mut pane, &rx, "the jump was never answered", &mut answers, |_, n| n > 0);
    assert_eq!(pane.view_offset(), 0, "the late answer took the pane back into history");
    assert!(pane.prompt_flash_until().is_none(), "the late answer marked a row");
    // the session is live too: a frame with the jump's line on top never comes
    std::thread::sleep(Duration::from_millis(300));
    pump(&mut pane, &rx, &mut answers);
    assert_ne!(top_row(&pane), "$ cmd3", "the session stayed where the jump went");

    pane.scroll_own_view(third as i32);
    wait(&mut pane, &rx, "a scroll to where the jump went was not shown", |p| top_row(p) == "$ cmd3");
    std::mem::forget(pane);
}
