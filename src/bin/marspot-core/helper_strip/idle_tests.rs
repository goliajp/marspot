//! A batch whose helpers are all idle, as a core that started after
//! they ended their turns hears of it.

use super::height::Said;
use super::host::Host;
use super::host_tests::{app, gone, given, say, turned_on};
use crate::boot_assembly_tests::Sandbox;
use marspot::shell_proto::{HelperStrip, StripRow, StripState};

fn record(rows: Vec<StripRow>) -> HelperStrip {
    HelperStrip { batch: 7, rows, ..HelperStrip::empty(9) }
}

#[test]
fn a_batch_heard_of_all_idle_takes_rows_and_one_all_ended_does_not() {
    let mut h = Host::default();
    let idle = record(vec![gone(1, false), gone(2, false)]);
    h.heard(&idle);
    assert_eq!(h.said(Some(&idle), true, false), Said::Rows { works: false, need: 4 });
    let mut h = Host::default();
    let exited = record([1, 2].map(|sid| StripRow { state: StripState::Exited, ..gone(sid, false) }).into());
    h.heard(&exited);
    assert_eq!(h.said(Some(&exited), true, false), Said::Rows { works: false, need: 0 }, "they go in a moment");
}

#[test]
fn a_fresh_core_gives_idle_helpers_their_rows_and_gives_them_again_after_losing_them() {
    let sb = Sandbox::new();
    let (mut app, s) = app(&sb);
    say(&mut app, s.sid, vec![gone(4, false), gone(5, false)]);
    assert_eq!(given(&app), 4, "never seen at work by this core");
    turned_on(false);
    app.strips_tick();
    assert_eq!(given(&app), 0);
    turned_on(true);
    app.strips_tick();
    assert_eq!(given(&app), 4, "the strip comes back while the shell lists them");
}
