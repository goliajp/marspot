//! Text watches against output parsed in one batch that moves the
//! cursor up: a slow turn of the loop takes in a `clear`, what a
//! command printed after it and the next prompt together.

use marspot_term::shell_proto::{PaneWatch, WatchBody, WatchKind};

use crate::SessionImpl;
use crate::local_session::LocalSession;
use crate::pane_ctl::{PaneCtl, Request};
use crate::pty_reader::Chunk;

struct Pane {
    session: SessionImpl,
    tx: std::sync::mpsc::Sender<Chunk>,
    ctl: PaneCtl,
}

impl Pane {
    fn new() -> Self {
        let (local, tx) = LocalSession::parsing_only(40, 10);
        let session = SessionImpl::Local(local);
        let (wake, _woken) = std::sync::mpsc::channel();
        let ctl = PaneCtl::new(&session, Default::default(), wake);
        Self { session, tx, ctl }
    }

    /// One turn of the loop with `out` to parse.
    fn turn(&mut self, out: &str) {
        self.turn_in(&[out]);
    }

    /// One turn of the loop with `chunks` to parse, read apart.
    fn turn_in(&mut self, chunks: &[&str]) {
        for out in chunks {
            self.tx.send(out.as_bytes().to_vec().into()).expect("queue a chunk");
        }
        self.session.pump();
        self.ctl.after_turn(&mut self.session, None, false, None);
    }

    fn pos(&self) -> u64 {
        crate::pane_ctl::local(&self.session).output_pos()
    }

    fn watch(&mut self, since_pos: u64, text: &str) {
        self.watch_as(1, since_pos, text);
    }

    fn cancel(&mut self, watch_id: u32) {
        self.ask(WatchBody { watch_id, kind: WatchKind::Cancel, since_pos: 0, quiet_ms: 0, text: String::new() });
    }

    fn watch_as(&mut self, watch_id: u32, since_pos: u64, text: &str) {
        self.ask(WatchBody { watch_id, kind: WatchKind::Text, since_pos, quiet_ms: 0, text: text.into() });
    }

    fn ask(&mut self, watch: WatchBody) {
        self.ctl.request(Request::Watch(PaneWatch { req_id: 1, sid: 1, watch }), &mut self.session, None);
    }
}

const BEFORE: &str = "% printf\r\nneedle-xyz\r\n% ";
const CLEARED: &str = "clear; printf\r\n\x1b[H\x1b[2Jneedle-xyz\r\n% ";

#[test]
fn watch_text_printed_above_where_a_batch_began_and_ended_matches() {
    let mut p = Pane::new();
    p.turn(BEFORE);
    let since = p.pos();
    p.watch(since, "needle-xyz");
    assert_eq!(p.ctl.met(), None, "the text was there before the position");
    p.turn(CLEARED);
    assert_eq!(p.ctl.met(), Some(Some(0)), "the line the batch went up to");
}

#[test]
fn watch_text_printed_below_where_a_batch_ended_matches() {
    let mut p = Pane::new();
    p.turn("% ");
    let since = p.pos();
    p.watch(since, "status-line");
    p.turn("\x1b[8;1Hstatus-line\x1b[2;1H");
    assert_eq!(p.ctl.met(), Some(Some(7)));
}

#[test]
fn watch_text_above_every_row_a_batch_wrote_on_does_not_match() {
    let mut p = Pane::new();
    p.turn(BEFORE);
    let since = p.pos();
    p.watch(since, "needle-xyz");
    p.turn("out\r\n\x1b[6;1Hmore\x1b[3;1H% ");
    assert_eq!(p.ctl.met(), None);
}

/// Old text on rows a batch goes up over, the cursor ending below.
const OLD: &str = "needle-xyz\r\none\r\ntwo\r\n% ";

fn never_matches(turns: &[&str]) {
    let mut p = Pane::new();
    p.turn(OLD);
    let since = p.pos();
    p.watch(since, "needle-xyz");
    for out in turns {
        p.turn(out);
        assert_eq!(p.ctl.met(), None, "after {out:?}");
    }
}

#[test]
fn watch_old_text_passed_over_to_rewrite_a_row_does_not_match() {
    never_matches(&["\x1b[3A\x1b[1B\rone z\x1b[2B\r% "]);
}

#[test]
fn watch_old_text_passed_over_by_a_cursor_put_back_does_not_match() {
    never_matches(&["\x1b7\x1b[H\x1b[2Bx\x1b8"]);
    never_matches(&["\x1b7\x1b[6;30Hx\x1b[H\x1b8"]);
}

#[test]
fn watch_text_of_another_watch_does_not_widen_this_one_s_rows() {
    let mut p = Pane::new();
    p.turn(OLD);
    let since = p.pos();
    p.watch(since, "needle-xyz");
    p.watch_as(2, since, "other");
    p.turn("\x1b7\x1b[2;1Hx\x1b[H\x1b8other");
    assert_eq!(p.ctl.met(), Some(Some(3)), "the first met is the other's");
    p.cancel(2);
    assert_eq!(p.ctl.met(), None);
}

#[test]
fn watch_rows_of_the_alternate_screen_are_not_the_main_one_s() {
    let mut p = Pane::new();
    p.turn(OLD);
    let since = p.pos();
    p.turn("\x1b[?1049h\x1b[Hup here\r\n");
    p.turn("\x1b[H\x1b[?1049l% ");
    p.watch(since, "needle-xyz");
    assert_eq!(p.ctl.met(), None);
    p.turn("more\r\n% ");
    assert_eq!(p.ctl.met(), None);
}

#[test]
fn watch_rows_of_the_alternate_screen_are_not_read_when_it_is_left() {
    // a screen of history, so that the two screens' numbers differ
    let mut p = Pane::new();
    p.turn(&format!("needle-xyz\r\n{}% ", "line\r\n".repeat(30)));
    let since = p.pos();
    p.watch(since, "needle-xyz");
    p.turn("\x1b[?1049h\x1b[5;1Hup here\x1b[H");
    p.turn("\x1b[?1049l% ");
    assert_eq!(p.ctl.met(), None);
}

#[test]
fn watch_text_printed_before_the_alternate_screen_matches_when_it_is_left() {
    let mut p = Pane::new();
    p.turn("% ");
    let since = p.pos();
    p.watch(since, "shown-first");
    p.turn(&format!("edit\r\nshown-first\r\n\x1b[?1049h{}", "up here\r\n".repeat(30)));
    assert_eq!(p.ctl.met(), None, "not on the alternate screen");
    p.turn("\x1b[?1049l% ");
    assert_eq!(p.ctl.met(), Some(Some(1)));
}

#[test]
fn watch_text_printed_before_a_watch_on_the_alternate_screen_does_not_match() {
    let mut p = Pane::new();
    p.turn("% ");
    p.turn("edit\r\nshown-first\r\n\x1b[?1049h\x1b[Hup here");
    let since = p.pos();
    p.watch(since, "shown-first");
    p.turn("\x1b[?1049l% ");
    assert_eq!(p.ctl.met(), None);
}

/// Four rows of old output, the cursor on the fifth.
const FOUR: &str = "a\r\nb\r\nc\r\nd\r\n% ";

#[test]
fn watch_text_printed_above_in_the_batch_that_enters_the_alternate_screen_matches_when_it_is_left() {
    let mut p = Pane::new();
    p.turn(FOUR);
    let since = p.pos();
    p.watch(since, "needle-xyz");
    p.turn("\x1b[H\x1b[2Jneedle-xyz\r\n\x1b[?1049h\x1b[Hup here");
    assert_eq!(p.ctl.met(), None, "not on the alternate screen");
    p.turn("\x1b[?1049l% ");
    assert_eq!(p.ctl.met(), Some(Some(0)));
}

#[test]
fn watch_text_drawn_on_the_alternate_screen_does_not_widen_the_main_one_s_rows() {
    let mut p = Pane::new();
    p.turn(OLD);
    let since = p.pos();
    p.watch(since, "needle-xyz");
    p.turn("\x1b[?1049h\x1b[Hneedle-xyz\x1b[?1049l\x1b[3A\x1b[3B");
    assert_eq!(p.ctl.met(), None);
}

#[test]
fn watch_text_split_by_an_escape_that_unread_bytes_end_in_matches() {
    let mut p = Pane::new();
    p.turn(FOUR);
    let since = p.pos();
    p.watch(since, "needle-xyz");
    // no `z` and a last line too long to read: the chunk is skipped
    let first = format!("\x1b[H\x1b[2Jneedle-x{}y\x1b[", "\x1b[0m".repeat(130));
    p.turn_in(&[&first, "1mz\r\n\x1b[5;1H% "]);
    assert_eq!(p.ctl.met(), Some(Some(0)));
}
