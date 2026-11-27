//! The pane follows the offset its session publishes at, except while
//! one of its own view asks is still unanswered.  The frames come from
//! a real region written the way a session writes it.

use std::os::unix::net::UnixStream;

use super::*;
use crate::grid_shm::{GridShmReader, GridShmWriter, SessionRegions};
use crate::shell_proto::view_ask_seq;
use crate::terminal::Terminal;

fn pane_on(writer: &GridShmWriter) -> Pane {
    let fd = unsafe { libc::dup(writer.fd()) };
    assert!(fd >= 0, "dup of the shm fd");
    let reader = GridShmReader::from_fd(fd).expect("shm reader");
    let (control, _peer) = UnixStream::pair().expect("socketpair");
    let (_tx, selection_rx) = std::sync::mpsc::channel();
    let id = 0xC3_0000_0000 + std::process::id() as u64;
    // our own pid: `reattach` never signals it
    let conn = L3Conn::reattach(std::process::id() as i32, control, reader, SessionRegions::for_session(id), selection_rx, id);
    Pane::over(PaneBackend::L3(conn))
}

/// A terminal with history to scroll into, and one more line printed
/// per call of `print`.
struct Program(Terminal);

impl Program {
    fn new() -> Self {
        let mut t = Terminal::new(20, 4);
        for n in 0..60 {
            t.feed(format!("line {n}\r\n").as_bytes());
        }
        Self(t)
    }

    fn print(&mut self, w: &mut GridShmWriter, view_offset: u16) {
        self.0.feed(b"more\r\n");
        w.publish(self.0.grid(), view_offset, 0);
    }
}

fn asked_for(pane: &Pane) -> Option<u16> {
    match pane.session() {
        PaneBackend::L3(c) => c.req_view_offset,
        _ => None,
    }
}

/// The program prints while the person sits in history: the session
/// raises its offset to keep the same lines in view and the pane takes
/// the raised offset, so the next wheel step starts from it.
#[test]
fn sitting_in_history_the_pane_follows_the_sessions_offset() {
    let mut w = GridShmWriter::create(20, 4).unwrap();
    let mut prog = Program::new();
    w.publish(prog.0.grid(), 0, 0);
    let mut pane = pane_on(&w);
    assert_eq!(pane.pump(), 1);
    assert_eq!(pane.view_offset(), 0);

    assert_eq!(pane.scroll_own_view(10), 10);
    // the session applies the scroll, and three lines go into history
    w.set_view_seq(view_ask_seq(1));
    for off in 11..=13 {
        prog.print(&mut w, off);
    }
    assert_eq!(pane.pump(), 1);
    assert_eq!(pane.view_offset(), 13);
    assert_eq!(asked_for(&pane), Some(13), "a scroll back to 13 would be sent again");
    prog.print(&mut w, 14);
    pane.pump();
    assert_eq!(pane.view_offset(), 14, "output with no ask in flight is followed too");
    assert_eq!(pane.scroll_own_view(-1), -1);
    assert_eq!(pane.view_offset(), 13, "the wheel step starts from where the session is");
}

/// The race: the person scrolls, and before the session reads the ask
/// it publishes another frame at its old offset.  That frame does not
/// answer the ask, so the pane keeps the offset it asked for; the
/// frame that does answer it is taken.
#[test]
fn a_frame_from_before_the_scroll_was_applied_does_not_undo_it() {
    let mut w = GridShmWriter::create(20, 4).unwrap();
    let mut prog = Program::new();
    w.set_view_seq(view_ask_seq(7));
    w.publish(prog.0.grid(), 20, 0);
    let mut pane = pane_on(&w);
    pane.pump();
    assert_eq!(pane.view_offset(), 20, "a core attaching takes where the session is");

    assert_eq!(pane.scroll_own_view(-15), -15);
    prog.print(&mut w, 21);
    assert_eq!(pane.pump(), 1, "the frame was read");
    assert_eq!(pane.view_offset(), 5, "a frame from before the ask took the pane back");

    w.set_view_seq(last_ask(&pane));
    prog.print(&mut w, 6);
    pane.pump();
    assert_eq!(pane.view_offset(), 6);
}

/// A session from before the ask numbers publishes 0 for every frame:
/// once the pane has asked for anything, it keeps its own offset, as
/// it always did.
#[test]
fn a_session_that_does_not_number_its_frames_is_not_followed() {
    let mut w = GridShmWriter::create(20, 4).unwrap();
    let mut prog = Program::new();
    w.publish(prog.0.grid(), 0, 0);
    let mut pane = pane_on(&w);
    pane.pump();
    pane.scroll_own_view(10);
    prog.print(&mut w, 11);
    pane.pump();
    assert_eq!(pane.view_offset(), 10);
}

fn last_ask(pane: &Pane) -> u16 {
    match pane.session() {
        PaneBackend::L3(c) => c.view_sync.asked.expect("an ask was sent"),
        _ => panic!("not an L3 pane"),
    }
}

/// A core attaching to a session that a core before it scrolled: the
/// session still publishes the number of that core's last ask.  The
/// new core's asks never carry that number, so frames published
/// before the session applies one of them do not pull the pane back,
/// wherever the number is, wraparound included.
#[test]
fn after_reattach_a_stale_frame_does_not_pull_the_pane_back() {
    for left in [view_ask_seq(0), view_ask_seq(1), view_ask_seq(2), 0xFFFE, 0xFFFF] {
        let mut w = GridShmWriter::create(20, 4).unwrap();
        let mut prog = Program::new();
        w.set_view_seq(left);
        w.publish(prog.0.grid(), 30, 0);
        let mut pane = pane_on(&w);
        pane.pump();
        assert_eq!(pane.view_offset(), 30, "seq {left}: a core attaching takes where the session is");

        let mut at: u16 = 30;
        for step in 0..3u16 {
            assert_eq!(pane.scroll_own_view(-3), -3);
            at -= 3;
            assert_ne!(last_ask(&pane), left, "seq {left}: ask {step} carries the old core's number");
            // the session prints before it reads the ask
            prog.print(&mut w, 31 + step);
            assert_eq!(pane.pump(), 1);
            assert_eq!(pane.view_offset(), at, "seq {left}: a stale frame pulled the pane back at ask {step}");
            assert_eq!(asked_for(&pane), Some(at), "seq {left}: the asked offset was pulled back");
        }

        w.set_view_seq(last_ask(&pane));
        prog.print(&mut w, at + 1);
        pane.pump();
        assert_eq!(pane.view_offset(), at + 1, "seq {left}: the frame that answers the ask is followed");
    }
}
