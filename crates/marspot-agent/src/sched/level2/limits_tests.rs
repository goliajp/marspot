use super::*;

#[test]
fn sched_limits_steady_is_the_set_the_planner_was_proved_with() {
    let s = Limits::STEADY;
    assert_eq!((s.dwell_h, s.lasts_h, s.per_week, s.fleet_per_day, s.hops, s.notices_per_hour, s.inertia), (6.0, 6.0, 6, 8, 2, 4, 0.25));
    assert_eq!(s.switch_extra, 0.0);
    assert_eq!(s.pi(0.4), 0.4);
    assert_eq!(Limits::default(), Limits::BALANCED);
    assert_eq!(Limits::BALANCED.pi(0.4), 1.0);
    assert_eq!(Limits::BALANCED.pi(0.0), 0.0, "no gap, no full price");
}

#[test]
fn sched_limits_overrides_apply_in_range_and_change_the_digest() {
    let b = Limits::BALANCED;
    let o = b.with("min_stay_h", 2.0).with("switch_extra_points", 0.5).with("per_day", 4.0);
    assert_eq!((o.dwell_h, o.per_day), (2.0, 4));
    assert!((o.switch_extra - 0.005).abs() < 1e-15);
    assert_ne!(o.digest(), b.digest());
    for (k, v) in [("min_stay_h", -1.0), ("per_day", 2.5), ("hops", 9.0), ("inertia", 3.0), ("nonsense", 1.0)] {
        assert_eq!(b.with(k, v), b, "{k} = {v}");
    }
    assert_eq!(o.words(), format!("profile=balanced limits={:08x}", o.digest() as u32));
}

#[test]
fn sched_limits_switches_left_counts_week_and_day() {
    assert_eq!(Limits::STEADY.switches_left(4, 9), 2);
    assert_eq!(Limits::BALANCED.switches_left(2, 2), 1);
    assert_eq!(Limits::BALANCED.switches_left(14, 0), 0);
    assert_eq!(Limits::FULL.switches_left(100, 5), 1);
}

#[test]
fn sched_limits_digests_differ_by_profile() {
    let d: Vec<u64> = [Limits::STEADY, Limits::BALANCED, Limits::FULL].iter().map(Limits::digest).collect();
    assert!(d[0] != d[1] && d[1] != d[2] && d[0] != d[2]);
}
