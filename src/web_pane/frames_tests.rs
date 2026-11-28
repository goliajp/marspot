use super::*;
use madcawl_wire::display::{self, Buffer, PxRect};
use madcawl_wire::{tag, Range};

const KIND: u16 = host::kind::HELLO;

fn answer(min: u32, cur: u32, version: Option<u32>) -> Frame {
    Hello { range: Range::new(min, cur), role: Some(host::role::DISPLAY), version }.frame(KIND)
}

#[test]
fn a_hello_answer_agrees_or_says_which_side_is_too_old() {
    let ours = Range::new(1, 2);
    assert_eq!(hello_answer(&answer(1, 1, Some(1)), KIND, ours), Ok(1));
    assert_eq!(hello_answer(&answer(2, 5, Some(2)), KIND, ours), Ok(2));
    assert_eq!(hello_answer(&answer(0, 0, None), KIND, Range::new(1, 1)), Err(Unspoken::TooOld(Some(Range::new(0, 0)))));
    assert_eq!(hello_answer(&answer(3, 4, None), KIND, ours), Err(Unspoken::TooNew(Range::new(3, 4))));
}

#[test]
fn a_host_from_before_the_hello_is_too_old_and_a_bad_answer_is_not_taken() {
    let ours = Range::new(1, 1);
    let refusal = Frame::new(host::kind::ERROR).num(tag::CODE, 6).str(tag::TEXT, "unknown request kind");
    assert_eq!(hello_answer(&refusal, KIND, ours), Err(Unspoken::TooOld(None)));
    assert!(matches!(hello_answer(&Frame::new(999), KIND, ours), Err(Unspoken::Bad(_))));
    // ranges that meet, but the answer names no version or another one
    assert!(matches!(hello_answer(&answer(1, 1, None), KIND, ours), Err(Unspoken::Bad(_))));
    let missing_cur = Frame::new(KIND).num(tag::MIN, 1);
    assert!(matches!(hello_answer(&missing_cur, KIND, ours), Err(Unspoken::Bad(_))));
}

fn surface(epoch: u64) -> Surface {
    let (w, h) = (100, 50);
    let b = Buffer { bytes: buffer_bytes(w, h, 16384), width: w, height: h, stride: madcawl_wire::shm::stride_for(w) };
    Surface { epoch, scale: 100, buffers: vec![b, b] }
}

fn commit(epoch: u64, seq: u64, buffer: u32) -> Commit {
    Commit { epoch, seq, buffer, dirty: PxRect { x: 0, y: 0, w: 100, h: 50 }, doc_width: 100, doc_height: 50 }
}

fn marked(seq: u64) -> impl Fn(usize) -> u64 {
    move |_| seq
}

#[test]
fn commits_are_shown_and_their_buffers_go_back_once_a_newer_one_was_drawn() {
    let mut c = Commits::new(surface(5));
    assert_eq!(c.take(&commit(5, 1, 0).frame(), marked(1)), Verdict::Show { buffer: 0, seq: 1 });
    let mut out = Vec::new();
    c.release(1, &mut out);
    assert!(out.is_empty(), "the buffer shown is kept");
    assert_eq!(c.take(&commit(5, 2, 1).frame(), marked(2)), Verdict::Show { buffer: 1, seq: 2 });
    c.release(1, &mut out);
    assert!(out.is_empty(), "commit 1 may still be on screen until a frame of 2 finished");
    c.release(2, &mut out);
    assert_eq!(out, vec![1]);
    out.clear();
    c.release(2, &mut out);
    assert!(out.is_empty(), "released once");
}

#[test]
fn every_out_of_range_commit_is_refused_and_three_in_a_row_close_the_channel() {
    let mut c = Commits::new(surface(5));
    assert_eq!(c.take(&commit(5, 4, 0).frame(), marked(4)), Verdict::Show { buffer: 0, seq: 4 });
    let refused = |v: Verdict| match v {
        Verdict::Refused { why, close } => (why, close),
        other => panic!("taken: {other:?}"),
    };
    assert_eq!(refused(c.take(&commit(5, 4, 1).frame(), marked(4))), ("sequence not after the last", false));
    assert_eq!(refused(c.take(&commit(5, 5, 0).frame(), marked(5))), ("the buffer being shown", false));
    assert_eq!(refused(c.take(&commit(5, 6, 2).frame(), marked(6))), ("no such buffer", true));
    let mut c = Commits::new(surface(5));
    let mut wide = commit(5, 1, 1);
    wide.dirty = PxRect { x: 1, y: 0, w: 100, h: 50 };
    assert_eq!(refused(c.take(&wide.frame(), marked(1))).0, "dirty rectangle outside the buffer");
    let mut empty = commit(5, 1, 1);
    empty.dirty.h = 0;
    assert_eq!(refused(c.take(&empty.frame(), marked(1))).0, "dirty rectangle outside the buffer");
    let mut tall = commit(5, 1, 1);
    tall.doc_height = display::MAX_DOC + 1;
    assert!(refused(c.take(&tall.frame(), marked(1))).1);
    // a good commit clears the count
    let mut c = Commits::new(surface(5));
    c.take(&commit(5, 1, 3).frame(), marked(1));
    c.take(&commit(5, 1, 3).frame(), marked(1));
    assert_eq!(c.take(&commit(5, 1, 0).frame(), marked(1)), Verdict::Show { buffer: 0, seq: 1 });
    assert_eq!(refused(c.take(&commit(5, 1, 1).frame(), marked(1))), ("sequence not after the last", false));
}

#[test]
fn a_commit_without_its_pixels_marked_or_malformed_is_refused() {
    let mut c = Commits::new(surface(5));
    let v = c.take(&commit(5, 1, 0).frame(), marked(0));
    assert_eq!(v, Verdict::Refused { why: "pixels not marked written", close: false });
    let mut f = commit(5, 1, 0).frame();
    f.fields.retain(|(t, _)| *t != tag::DIRTY);
    assert_eq!(c.take(&f, marked(1)), Verdict::Refused { why: "malformed", close: false });
}

#[test]
fn an_earlier_surfaces_commit_and_unknown_kinds_are_passed_over() {
    let mut c = Commits::new(surface(5));
    assert_eq!(c.take(&commit(4, 1, 0).frame(), marked(1)), Verdict::Pass);
    assert_eq!(c.take(&Frame::new(77).num(1, 1), marked(1)), Verdict::Pass);
    // neither counts towards closing
    for seq in 1..=3 {
        assert_eq!(c.take(&commit(4, seq, 0).frame(), marked(seq)), Verdict::Pass);
    }
    assert_eq!(c.take(&commit(5, 1, 0).frame(), marked(1)), Verdict::Show { buffer: 0, seq: 1 });
}

#[test]
fn the_page_fills_the_pane_in_whole_css_px_and_buffers_are_whole_pages() {
    assert_eq!(page_size(1001.0, 701.0, 100), Some(((1001, 701), (1001, 701))));
    assert_eq!(page_size(1001.0, 701.0, 200), Some(((500, 350), (1000, 700))));
    assert_eq!(page_size(1.0, 701.0, 200), None);
    assert_eq!(page_size(f64::NAN, 10.0, 100), None);
    assert_eq!(page_size(40000.0, 10.0, 100), None);
    let b = buffer_bytes(1000, 700, 16384);
    assert_eq!(b % 16384, 0);
    assert!(b >= Header::needed(1000, 700) && b - Header::needed(1000, 700) < 16384);
    // the size every surface header check takes
    let stride = madcawl_wire::shm::stride_for(1000);
    assert!(madcawl_wire::shm::fits(1000, 700, stride, b).is_ok());
}
