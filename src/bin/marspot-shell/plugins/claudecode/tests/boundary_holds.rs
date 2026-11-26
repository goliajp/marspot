use super::*;
use crate::plugins::claudecode::worker_loop::holds;

#[test]
fn allocation_boundary_holds_child_and_timer_are_unknown_when_the_process_tree_was_not_read() {
    let mut r = scan_with(7, 4242, "");
    r.new_vetoes.insert(7, (true, false));
    let h = holds(&r, Some(7))[&7];
    assert_eq!((h.child, h.timer, h.focus), (Some(true), Some(false), true));
    r.new_vetoes.clear();
    let h = holds(&r, None)[&7];
    assert_eq!((h.child, h.timer, h.focus), (None, None, false), "no process info is not \"nothing runs\"");
}
