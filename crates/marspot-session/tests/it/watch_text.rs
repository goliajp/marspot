//! Text watches on what was written after a position: not what was on
//! screen before it, lines that scrolled away meanwhile, lines the
//! width wrapped; and the position across an image swap.

use marspot_term::shell_proto::{WatchKind, WatchReason};
use marspot_term::test_env::sandbox_state_dir;

use crate::pane_link::{Link, abs_row_exactly, abs_row_of, body, pushed, screen_shows, text, wait, with_zsh};
use crate::support::{COLS, ROWS};

#[test]
fn watch_text_already_on_screen_waits_for_new_output() {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    l.inject("printf 'needle-%s\\n' xyz\r");
    screen_shows(&s, "needle-xyz");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let p = l.pos();
    l.armed(text(1, p, "needle-xyz"));
    assert!(l.try_fired(1, 0.5).is_none(), "the text was there before the position");
    l.inject("clear; printf 'needle-%s\\n' xyz\r");
    let f = l.fired(1, 5.0);
    let mut row = None;
    wait(5.0, "the new line on screen", || {
        row = abs_row_of(&s, "needle-xyz");
        row.is_some()
    });
    assert_eq!(f.line, row, "the matched line is the new one");
}

#[test]
fn watch_text_scrolled_into_the_history_before_registration_matches() {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    let p = l.pos();
    let before = pushed(&s);
    l.inject("printf 'deep-%s\\n' marker; seq 1 100\r");
    screen_shows(&s, "100");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(s.screen(COLS, ROWS).iter().all(|r| !r.contains("deep-marker")), "it scrolled away");
    l.armed(text(1, p, "deep-marker"));
    let f = l.fired(1, 1.0);
    let line = f.line.expect("a line");
    assert!(line < pushed(&s), "the line is in the history: {line}");
    // the command's echo is one row, and its first line of output the next
    assert!(line > before, "{line} is a line written after the position ({before})");
}

#[test]
fn watch_text_matches_a_line_the_width_wrapped() {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    let p = l.pos();
    l.armed(text(1, p, "wrapneedle"));
    // 75 zeros first: the text crosses the 80th column
    l.inject("printf '%075d%s%s\\n' 0 wrap needle\r");
    let f = l.fired(1, 5.0);
    let mut row = None;
    wait(5.0, "the line on screen", || {
        row = abs_row_of(&s, &"0".repeat(70));
        row.is_some()
    });
    assert_eq!(f.line, row, "the line's first row");
}

#[test]
fn watch_position_and_command_end_survive_an_image_swap() {
    let mut sandbox = crate::self_execv::Sandbox::new();
    let s = crate::self_execv::start(&mut sandbox);
    let mut l = Link::new(&s);
    l.inject("printf '\\033]133;D;%s\\007' 3\r");
    let p0 = l.pos();
    wait(5.0, "the command end", || l.pos() > p0);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let before = l.pos();
    l.armed(body(1, WatchKind::Change, u64::MAX - 1));
    crate::self_execv::stage(sandbox.path(), true);
    s.send_frame(marspot_term::shell_proto::MsgType::SessionUpdate, Vec::new());
    assert_eq!(l.fired(1, 30.0).reason, WatchReason::Restarting, "told before the image went");
    let pid = s.pid() as i32;
    wait(30.0, "the new image", || {
        crate::self_execv::count_in_log(sandbox.path(), "L3_EXECV_RESUMED") >= 1 && unsafe { libc::kill(pid, 0) } == 0
    });
    let mut l = Link::on(
        crate::support::reattach(&marspot_term::session_registry::session_socket_path(crate::self_execv::id()), 15)
            .expect("reattach after the swap"),
    );
    let after = l.pos();
    assert!(after >= before, "the position went back across the swap: {before} -> {after}");
    l.armed(body(2, WatchKind::CommandEnd, p0));
    let f = l.fired(2, 2.0);
    assert_eq!(f.status, Some(3), "the command end before the swap is still known");
    assert!(f.pos > p0 && f.pos <= before);
}

#[test]
fn watch_text_in_the_commands_own_echo_waits_for_its_output() {
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    let command = "sleep 1; echo ECHO-DONE";
    let req = l.type_in(command.as_bytes(), true, true, true, Some(text(1, 0, "ECHO-DONE")));
    assert_eq!(l.typed(req, 5.0).result, marspot_term::shell_proto::TypedResult::Written);
    assert!(l.try_fired(1, 0.7).is_none(), "the command line holds the text, the output does not yet");
    let f = l.fired(1, 5.0);
    // the output's own row, found by itself: behind a wide prompt the
    // command line wraps over two rows
    let mut row = None;
    wait(5.0, "the output on screen", || {
        row = abs_row_exactly(&s, "ECHO-DONE");
        row.is_some()
    });
    assert_eq!(f.line, row, "the matched line is the output's, not the command line's");
}

#[test]
fn watch_text_printed_while_the_grid_is_held_matches_once_it_is_let_go() {
    use marspot_term::shell_proto::{MsgType, encode_pane_hold_grid};
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    let p = l.pos();
    l.armed(text(1, p, "held-text"));
    s.send_frame(MsgType::PaneHoldGrid, encode_pane_hold_grid(0, true));
    l.inject("printf 'held-%s\\n' text\r");
    wait(5.0, "the output, counted while held", || l.pos() > p + 40);
    assert!(l.try_fired(1, 0.5).is_none(), "the held grid does not have the line yet");
    s.send_frame(MsgType::PaneHoldGrid, encode_pane_hold_grid(0, false));
    assert!(l.fired(1, 5.0).line.is_some(), "the line, once the grid has it");
}

/// The clear, the text and the next prompt reach the grid as one batch,
/// as they do for a session that did not get to run meanwhile.
#[test]
fn watch_text_printed_after_a_clear_matches_in_one_batch_with_the_prompt() {
    use marspot_term::shell_proto::{MsgType, encode_pane_hold_grid};
    let mut sandbox = sandbox_state_dir();
    let s = with_zsh(&mut sandbox);
    let mut l = Link::new(&s);
    l.inject("printf 'needle-%s\\n' xyz\r");
    screen_shows(&s, "needle-xyz");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let p = l.pos();
    l.armed(text(1, p, "needle-xyz"));
    s.send_frame(MsgType::PaneHoldGrid, encode_pane_hold_grid(0, true));
    l.inject("printf '\\033[H\\033[2Jneedle-%s\\n' xyz\r");
    wait(5.0, "the output, counted while held", || l.pos() > p + 60);
    let mut quiet = body(2, WatchKind::Quiet, p);
    quiet.quiet_ms = 300;
    l.armed(quiet);
    l.fired(2, 10.0);
    assert!(l.try_fired(1, 0.0).is_none(), "the held grid does not have the line yet");
    s.send_frame(MsgType::PaneHoldGrid, encode_pane_hold_grid(0, false));
    assert!(l.fired(1, 5.0).line.is_some());
}
