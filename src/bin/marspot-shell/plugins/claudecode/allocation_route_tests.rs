use super::*;
use marspot_agent::quota::Quant;
use marspot_agent::sched::level2::{Account, FLEET_PER_DAY, NOTICES_PER_HOUR, plan};

fn acct(id: u8, r: f64, t: f64) -> Account {
    Account { q: Quant { id, r, f: 0.0, t, tf: t, open: true, stale: false }, spare: r, price: [1.0, 1.0] }
}

fn input(a: &[Account]) -> Input<'_> {
    Input { accounts: a, panes: &[], young: [0.0; 2], window_full: 0.528, pi: 0.5, fleet_left: FLEET_PER_DAY, notices_left: NOTICES_PER_HOUR, stale_feed: false, limits: marspot_agent::sched::level2::Limits::STEADY }
}

// one test: the table is the process's one published value
#[test]
fn sched_route_shadow_places_a_batch_across_passes_and_logs_the_table() {
    let now = 1_800_000_000;
    assert_eq!(shadow_pane("pane", 1, Class::Plain, 0.004, None, Some(2), now).0, "kind=pane sid=1 class=plain today=P2 table=-", "no table before a pass");
    // P2 renews in 10 h with 0.5 points: the first helpers go there
    let a = [acct(2, 0.005, 10.0), acct(4, 0.6, 120.0)];
    let i = input(&a);
    rebuild(&i, &plan(&i, None), None, None, now);
    assert_eq!(line().as_deref(), Some("fable=- plain=P2=0.5,P4=60.0 others=-"));
    let placed: Vec<String> = (0..6).map(|_| place(Class::Plain, RHO0, None, None, None, None, now).0).collect();
    assert!(placed[0].starts_with("table=P2 ") && placed[4].starts_with("table=P4 "), "{placed:?}");
    // the next pass sees none of them yet: their use stays taken out
    rebuild(&i, &plan(&i, None), None, None, now + 2);
    let left = line().expect("a table");
    assert!(!left.contains("P4=60.0"), "the helpers on P4 stay counted: {left}");
    // once they are past their life they are the plan's to count
    rebuild(&i, &plan(&i, None), None, None, now + KEEP_S + 1);
    assert_eq!(line().as_deref(), Some("fable=- plain=P2=0.5,P4=60.0 others=-"));
    let later = now + KEEP_S + 2;
    let (moved, live) = shadow_pane("badge", 7, Class::Plain, 0.004, Some(2), Some(4), later);
    assert!(moved.starts_with("kind=badge sid=7 class=plain today=P4 table=P4 "), "{moved}");
    assert_eq!(live, Pick::None, "the table is not live");
    // every placement was noted for the ladder, none of them wrong
    assert_eq!(take_noted(), vec![false; 7]);
    assert!(take_noted().is_empty());
    // live: a moving pane goes where the table says; only a placement the
    // caller acts on is kept for the ladder's check of refusals after it
    set_live(true);
    let (_, live) = shadow_pane("refused", 8, Class::Plain, 0.004, Some(2), Some(2), later + 1);
    assert_eq!(live, Pick::To(4));
    assert!(live_placed(later + 1).is_empty(), "a pick the caller may still drop is not counted");
    used(4, later + 1);
    assert_eq!(live_placed(later + 1), vec![(later + 1, 4)]);
    assert!(live_placed(later + 3700).is_empty());
    set_live(false);
    take_noted();
    // a clicked pane is in the plan's input already: the next table does
    // not take its use out a second time
    rebuild(&i, &plan(&i, None), None, None, later + 2);
    assert_eq!(line().as_deref(), Some("fable=- plain=P2=0.5,P4=60.0 others=-"));
    // the other agents read on a full search stay in the line until the next read
    rebuild(&i, &plan(&i, None), None, Some(vec![("codex", 0.16)]), later + 4);
    rebuild(&i, &plan(&i, None), None, None, later + 6);
    assert_eq!(line().as_deref(), Some("fable=- plain=P2=0.5,P4=60.0 others=codex=16.0"));
    // a full pass's probes leave the table as it was; with no account whose
    // week is all Fable-usable a plain probe cannot go wrong and is not
    // counted, and a Fable probe never is
    take_noted();
    let probes = probe();
    assert_eq!(probes.len(), 2, "{probes:?}");
    assert!(probes[0].starts_with("kind=probe class=plain table=P") && probes[0].ends_with(" counted=0"), "{probes:?}");
    assert_eq!(probes[1], "kind=probe class=fable table=- counted=0", "no account has Fable-usable room");
    assert!(take_noted().is_empty());
    assert_eq!(line().as_deref(), Some("fable=- plain=P2=0.5,P4=60.0 others=codex=16.0"));
    // one whose only room is Fable-usable is a wrong placement, counted
    // once for the same table
    let f = [Account { q: Quant { id: 3, r: 0.4, f: 0.4, t: 100.0, tf: 100.0, open: true, stale: false }, spare: 0.4, price: [1.0, 1.0] }];
    let i = input(&f);
    rebuild(&i, &plan(&i, None), None, None, later + 8);
    let probes = probe();
    assert!(probes[0].starts_with("kind=probe class=plain table=P3 ") && probes[0].ends_with(" counted=1"), "{probes:?}");
    assert!(probes[1].starts_with("kind=probe class=fable table=P3 ") && probes[1].ends_with(" counted=0"), "{probes:?}");
    assert_eq!(take_noted(), vec![true]);
    rebuild(&i, &plan(&i, None), None, None, later + 70);
    assert!(probe()[0].ends_with(" counted=0"), "the same order of accounts again");
    assert!(take_noted().is_empty());
    // a plain account beside it: the probe goes there, right, on a new table
    let g = [f[0].clone(), acct(5, 0.3, 50.0)];
    let i = input(&g);
    rebuild(&i, &plan(&i, None), None, None, later + 130);
    assert!(probe()[0].starts_with("kind=probe class=plain table=P5 "));
    assert_eq!(take_noted(), vec![false]);
    assert!(live_placed(later + 130).is_empty(), "a probe is never a live placement");
    take_noted();
    // short: a day-long pane at a point an hour needs 24 of P6's 5 points;
    // the table places it anyway, marked, and keeps its use out
    let s = [acct(6, 0.05, 100.0)];
    let i = input(&s);
    rebuild(&i, &plan(&i, None), None, None, later + 200);
    assert_eq!(place(Class::Plain, 0.01, Some(H2), None, None, None, later + 200).0, "table=P6 room=5.00>-19.00 short=1");
    assert_eq!(take_noted(), vec![false], "a short placement counts for the ladder");
    rebuild(&i, &plan(&i, None), None, None, later + 202);
    assert_eq!(line().as_deref(), Some("fable=- plain=P6=-19.0 others=codex=16.0"), "the kept placement stays out, shown below zero");
    let probes = probe();
    assert!(probes[0].starts_with("kind=probe class=plain table=P6 room=-19.00>") && probes[0].ends_with(" short=1 counted=0"), "{probes:?}");
    assert!(take_noted().is_empty(), "no Fable-only account to pick wrongly");
    // live, short on two accounts: a refused pane short on today's account
    // is today's pick, not the table's; with no pick from today's rule it
    // goes nowhere; a clicked pane still goes short where it costs least
    let s = [acct(8, 0.05, 100.0), acct(9, 0.04, 100.0)];
    let i = input(&s);
    rebuild(&i, &plan(&i, None), None, None, later + 300);
    set_live(true);
    let (said, pick) = shadow_pane("refused", 9, Class::Plain, 0.01, Some(9), Some(8), later + 300);
    assert!(said.ends_with(" today=P8 table=P8 room=5.00>-19.00 short=1"), "{said}");
    assert_eq!(pick, Pick::None, "today's own account");
    let (said, pick) = shadow_pane("refused", 9, Class::Plain, 0.01, Some(9), None, later + 300);
    assert_eq!((said.as_str(), pick), ("kind=refused sid=9 class=plain today=- table=-", Pick::None));
    let (_, pick) = shadow_pane("badge", 9, Class::Plain, 0.01, Some(8), None, later + 300);
    assert_eq!(pick, Pick::To(9));
    set_live(false);
    take_noted();
    // protection with only a Fable-only account: other models keep working
    // there, and that placement is never counted as wrong
    let f = [Account { q: Quant { id: 3, r: 0.4, f: 0.4, t: 100.0, tf: 100.0, open: true, stale: false }, spare: 0.1, price: [1.0, 1.0] }];
    let i = Input { limits: marspot_agent::sched::level2::Limits::BALANCED, ..input(&f) };
    rebuild(&i, &plan(&i, None), None, None, later + 306);
    let probes = probe();
    assert!(probes[0].starts_with("kind=probe class=plain table=P3 room=10.00>") && probes[0].ends_with(" short=1 fable_only=1 counted=1"), "{probes:?}");
    assert_eq!(take_noted(), vec![false]);
}
