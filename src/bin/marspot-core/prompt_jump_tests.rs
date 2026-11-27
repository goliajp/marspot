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
/// read its newest frame.
fn pump(pane: &mut Pane, rx: &Receiver<CoreEvent>) {
    while let Ok(ev) = rx.try_recv() {
        if let CoreEvent::L3Says(_, frame) = ev {
            if frame.msg_type == MsgType::PromptJumped {
                pane.prompt_jumped(decode_prompt_jumped(&frame.payload).unwrap(), Instant::now());
            } else {
                pane.session_mut().l3_session_says(&frame);
            }
        }
    }
    pane.pump();
}

fn wait(pane: &mut Pane, rx: &Receiver<CoreEvent>, what: &str, done: impl Fn(&Pane) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        pump(pane, rx);
        if done(pane) {
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
