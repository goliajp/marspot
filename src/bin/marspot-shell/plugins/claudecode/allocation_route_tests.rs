use super::*;
use marspot_agent::quota::Quant;
use marspot_agent::sched::level2::{Account, FLEET_PER_DAY, NOTICES_PER_HOUR, plan};

fn acct(id: u8, r: f64, t: f64) -> Account {
    Account { q: Quant { id, r, f: 0.0, t, tf: t, open: true, stale: false }, spare: r, price: [1.0, 1.0] }
}

fn input(a: &[Account]) -> Input<'_> {
    Input { accounts: a, panes: &[], young: [0.0; 2], window_full: 0.528, pi: 0.5, fleet_left: FLEET_PER_DAY, notices_left: NOTICES_PER_HOUR, stale_feed: false }
}

// one test: the table is the process's one published value
#[test]
fn sched_route_shadow_places_a_batch_across_passes_and_logs_the_table() {
    let now = 1_800_000_000;
    assert_eq!(shadow_pane("pane", 1, Class::Plain, 0.004, None, Some(2), now), "kind=pane sid=1 class=plain today=P2 table=-", "no table before a pass");
    // P2 renews in 10 h with 0.5 points: the first helpers go there
    let a = [acct(2, 0.005, 10.0), acct(4, 0.6, 120.0)];
    let i = input(&a);
    rebuild(&i, &plan(&i, None), None, now);
    assert_eq!(line().as_deref(), Some("fable=- plain=P2=0.5,P4=60.0 others=-"));
    let placed: Vec<String> = (0..6).map(|_| place(Class::Plain, RHO0, None, None, now)).collect();
    assert!(placed[0].starts_with("table=P2 ") && placed[4].starts_with("table=P4 "), "{placed:?}");
    // the next pass sees none of them yet: their use stays taken out
    rebuild(&i, &plan(&i, None), None, now + 2);
    let left = line().expect("a table");
    assert!(!left.contains("P4=60.0"), "the helpers on P4 stay counted: {left}");
    // once they are past their life they are the plan's to count
    rebuild(&i, &plan(&i, None), None, now + KEEP_S + 1);
    assert_eq!(line().as_deref(), Some("fable=- plain=P2=0.5,P4=60.0 others=-"));
    let moved = shadow_pane("badge", 7, Class::Plain, 0.004, Some(2), Some(4), now);
    assert!(moved.starts_with("kind=badge sid=7 class=plain today=P4 table=P4 "), "{moved}");
}
