//! A jump to the prompt before or after the view: the session moves its
//! view so that the prompt's line is the top row, says where it went,
//! and does the same from the history on disk after its own update.

use marspot_term::shell_proto::{
    Frame, MsgType, PromptDir, decode_prompt_jumped, encode_grid_scroll, encode_inject_input, encode_prompt_jump,
};

use crate::self_execv::{Link, Sandbox, stage, start, wait_for};
use crate::support::{COLS, ROWS, Session};

/// Three commands of thirty lines each, every one behind a prompt the
/// shell marks with `OSC 133 ; A`, all of it far past one screen.
const RECORDING: &[u8] = b"\x15clear; for n in 1 2 3; do printf '\\033]133;A\\007$ cmd%s\\n' $n; \
    for i in $(seq 1 30); do echo out$n.$i; done; done; echo done.\r";

fn jump(dir: PromptDir) -> Frame {
    let mut payload = Vec::new();
    encode_prompt_jump(dir, &mut payload);
    Frame::new(MsgType::PromptJump, payload)
}

/// The top row of the newest frame, once that frame is at `off`.
fn top_at(s: &Session, off: u16) -> Option<String> {
    let (mut cells, mut wrapped) = (Vec::new(), Vec::new());
    let snap = s.reader.read_settled(&mut cells, &mut wrapped)?;
    (snap.view_offset == off).then(|| cells[..COLS as usize].iter().map(|c| c.ch).collect::<String>().trim_end().to_string())
}

/// Ask for a jump on the session's own socket and read where it went:
/// the frame published there has the prompt's line on top.
fn jump_to(s: &Session, dir: PromptDir, want: &str) -> u16 {
    let f = jump(dir);
    s.send_frame(f.msg_type, f.payload);
    let off = decode_prompt_jumped(&s.await_frame(MsgType::PromptJumped, 10).payload).expect("a jump's answer");
    let top = wait_for(10, &format!("the frame at {off}"), || top_at(s, off));
    assert_eq!(top, want, "the jump to offset {off} put another line on top");
    off
}

#[test]
fn a_jump_puts_the_prompt_on_the_top_row_and_still_does_after_an_update() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    let pid = s.pid();
    s.send_frame(MsgType::InjectInput, encode_inject_input(0, RECORDING));
    wait_for(10, "the recording and the prompt after it", || {
        let (rows, (col, _)) = s.try_screen_and_cursor(COLS, ROWS)?;
        (col > 0 && rows.iter().any(|l| l == "done.")).then_some(())
    });

    let third = jump_to(&s, PromptDir::Back, "$ cmd3");
    assert!(third > 0, "the last prompt is in the history, not on the live screen");
    let second = jump_to(&s, PromptDir::Back, "$ cmd2");
    assert!(second > third);
    jump_to(&s, PromptDir::Back, "$ cmd1");
    assert_eq!(jump_to(&s, PromptDir::Forward, "$ cmd2"), second);
    assert_eq!(jump_to(&s, PromptDir::Forward, "$ cmd3"), third);

    // no prompt after the last one: the view stays where it is
    let f = jump(PromptDir::Forward);
    s.send_frame(f.msg_type, f.payload);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(top_at(&s, third).as_deref(), Some("$ cmd3"), "a jump with nowhere to go moved the view");

    s.send_frame(MsgType::GridScroll, encode_grid_scroll(0));
    wait_for(10, "the live screen again", || top_at(&s, 0));
    let staged = stage(sandbox.path(), true).canonicalize().unwrap();
    s.send_frame(MsgType::SessionUpdate, Vec::new());
    wait_for(30, "the pid to run the staged image", || {
        assert_eq!(s.child.try_wait().unwrap(), None, "the L3 exited instead of handing over");
        let running = marspot_term::platform::pid_exe_path(pid as i32).and_then(|p| p.canonicalize().ok());
        (running.as_deref() == Some(staged.as_path())).then_some(())
    });
    // the new image reads the marks from the history file it took over
    let mut link = Link::open();
    let top = link.send_until(
        &s,
        "the frame the new image's jump published",
        |mut stream| {
            let _ = jump(PromptDir::Back).write_to(&mut stream);
        },
        || top_at(&s, third),
    );
    assert_eq!(top, "$ cmd3", "after the update the jump lands elsewhere");
    s.kill();
}
