use std::time::Duration;

use super::*;

fn resting() -> Pane {
    Pane {
        awaiting_user: true,
        user_here: false,
        work_in_flight: false,
        own_timer: false,
        idle_for: Duration::from_secs(600),
        since_last: None,
        move_in_flight: false,
        host_load: Some(4.0),
        helper_bound: false,
    }
}

#[test]
fn alloc_order_each_change_since_the_scan_holds_the_order() {
    assert_eq!(order_veto(&resting(), Kind::Clean, false), None);
    let cases = [
        (Pane { move_in_flight: true, ..resting() }, Veto::MoveOnItsWay),
        (Pane { awaiting_user: false, ..resting() }, Veto::Working),
        (Pane { user_here: true, ..resting() }, Veto::UserHere),
        (Pane { work_in_flight: true, ..resting() }, Veto::WorkInFlight),
        (Pane { own_timer: true, ..resting() }, Veto::OwnTimer),
        (Pane { host_load: Some(99.0), ..resting() }, Veto::Overloaded),
    ];
    for (pane, want) in cases {
        assert_eq!(order_veto(&pane, Kind::Short, false), Some(want));
    }
    assert_eq!(order_veto(&resting(), Kind::Clean, true), Some(Veto::RefusedThere));
    // a switch after a notice may end the work it warned of
    assert_eq!(order_veto(&Pane { work_in_flight: true, ..resting() }, Kind::Drain, false), None);
    assert_eq!(order_veto(&Pane { work_in_flight: true, ..resting() }, Kind::Notice, false), None);
}

#[test]
fn alloc_order_today_s_rule_keeps_its_waste_moves_unless_the_executor_acts_live() {
    assert!(!waste_is_today_s(true, Rebalance::Live));
    assert!(waste_is_today_s(true, Rebalance::DryRun));
    assert!(waste_is_today_s(true, Rebalance::Off));
    assert!(waste_is_today_s(false, Rebalance::Live));
}
