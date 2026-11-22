//! The bell frame: at most one per publish, and none for the BEL that
//! ends an OSC string.

use super::*;
use crate::local_session::{LocalSession, test_pane_env};
use std::os::unix::net::UnixStream;

/// Feed each slice to the pane's terminal and publish after each, as
/// the loop does; the bell frames the core would have received.
fn bells_sent(id: u64, feeds: &[&[u8]]) -> usize {
    let _env = test_pane_env("/bin/zsh");
    let mut session = SessionImpl::Local(LocalSession::spawn(id, 40, 5, "", || {}).expect("spawn"));
    let mut frames = Frames::new(GridShmWriter::create(40, 5).expect("a region"));
    let (ours, mut theirs) = UnixStream::pair().expect("a socket pair");
    let mut poke = ControlWriter::new(ours).expect("a writer");
    for f in feeds {
        session.terminal_mut().feed(f);
        frames.publish(&mut session, Some(&mut poke));
    }
    // the writer thread drains its queue and closes its end
    drop(poke);
    let mut bells = 0;
    while let Some(f) = Frame::read_from(&mut theirs).expect("a frame") {
        bells += usize::from(f.msg_type == MsgType::PaneBell);
    }
    bells
}

#[test]
fn many_bels_between_two_publishes_are_one_frame() {
    assert_eq!(bells_sent(0xB1, &[b"\x07\x07\x07ab\x07"]), 1);
}

#[test]
fn each_publish_with_a_bel_since_the_last_sends_one() {
    assert_eq!(bells_sent(0xB2, &[b"\x07", b"x", b"\x07\x07", b"y"]), 2);
}

#[test]
fn no_bel_no_frame() {
    assert_eq!(bells_sent(0xB3, &[b"plain", b"\r\nmore"]), 0);
}

#[test]
fn the_bel_that_ends_an_osc_string_sends_no_frame() {
    assert_eq!(bells_sent(0xB4, &[b"\x1b]2;a title\x07", b"\x1b]0;split", b"\x07"]), 0);
}

#[test]
fn a_frame_withheld_by_synchronized_output_does_not_hold_the_bell() {
    assert_eq!(bells_sent(0xB5, &[b"\x1b[?2026h\x07x"]), 1);
}
