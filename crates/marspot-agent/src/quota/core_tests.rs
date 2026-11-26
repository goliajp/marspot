//! The allocation's parts on small hand-made accounts.

use super::choose::{HELPER_LAST_H, Mover, loads_of, pick};
use super::notice::{Stranded, Told, stranded};
use super::plan::plan;
use super::replay::{Inputs, Kind, Why, replay};
use super::sim_policies::old_helper_pick;
use super::speed::{PaneUse, Slope, WINDOW_S};
use super::{Class, KAPPA_FABLE, Quant, RHO0, shortage};

fn acct(id: u8, r: f64, f: f64, t: f64) -> Quant {
    Quant { id, r, f, t, tf: t, open: true, stale: false }
}

#[test]
fn allocation_restricted_overflow_is_left_unassigned() {
    // one account with a little Fable: the rest of the Fable demand has nowhere to go
    let a = [acct(1, 1.0, 0.1, 10.0), acct(2, 0.5, 0.0, 20.0)];
    let p = plan(&a, 0.05, 0.0, Some(KAPPA_FABLE));
    assert!((p.t_f[0] - 0.01).abs() < 1e-12, "capped at Φ / T");
    assert_eq!(p.t_f[1], 0.0);
    assert!((p.unassigned_f - 0.04).abs() < 1e-12);
}

#[test]
fn allocation_reservation_and_renewal_requeue() {
    // P1 renews in 2 h: after that it is a fresh week and takes Fable again,
    // which counts against the next week, not this one
    let a = [acct(1, 0.1, 0.1, 2.0), acct(2, 1.0, KAPPA_FABLE, 100.0)];
    let p = plan(&a, 0.05, 0.0, Some(KAPPA_FABLE));
    assert!((p.t_f[0] - 0.05).abs() < 1e-12, "earliest renewal first, cap 0.05 an hour");
    assert!(p.d_f[0] <= 0.1 + 1e-12, "D_F stops at the renewal: {}", p.d_f[0]);
    assert!(p.d_f[1] > 0.0, "the later account takes what the renewed one cannot");
    assert!(p.d_f[1] < 0.05 * 168.0, "the renewed P1 takes Fable again in its new week");
    // U keeps D_F back from plain demand but never below N
    assert!((p.u[1] - (1.0 - p.d_f[1]).max(1.0 - KAPPA_FABLE)).abs() < 1e-12);
    // with no κ nothing is reserved
    let open = plan(&a, 0.05, 0.0, None);
    assert_eq!(open.d_f, vec![0.0, 0.0]);
    assert_eq!(open.u, vec![0.1, 1.0]);
}

#[test]
fn allocation_plain_overflow_split_by_u() {
    let a = [acct(1, 0.2, 0.0, 10.0), acct(2, 0.6, 0.0, 30.0)];
    let p = plan(&a, 0.0, 0.1, Some(KAPPA_FABLE));
    // caps 0.02 and 0.02; 0.06 left over, split 1:3
    assert!((p.overflow_n - 0.06).abs() < 1e-12);
    assert!((p.t_n[0] - (0.02 + 0.015)).abs() < 1e-12, "{:?}", p.t_n);
    assert!((p.t_n[1] - (0.02 + 0.045)).abs() < 1e-12, "{:?}", p.t_n);
}

#[test]
fn allocation_shortage_is_the_sum_of_r_over_t() {
    let a = [acct(1, 0.2, 0.0, 10.0), acct(2, 0.6, 0.0, 30.0), acct(3, 0.0, 0.0, 5.0)];
    assert!(!shortage(&a, 0.04));
    assert!(shortage(&a, 0.0401));
}

#[test]
fn allocation_all_out_is_said_once() {
    let a = [acct(1, 0.0, 0.0, 10.0), acct(2, 0.0, 0.0, 3.0)];
    let mut told = Told::default();
    let s = stranded(&a, Class::Plain);
    assert_eq!(s, Some(Stranded::AllOut { back_in_h: 3.0 }));
    assert_eq!(told.tell(7, s.clone()), s);
    assert_eq!(told.tell(7, s.clone()), None, "once per stretch");
    assert_eq!(told.tell(7, None), None);
    assert_eq!(told.tell(7, s.clone()), s, "a new stretch is said again");
}

#[test]
fn allocation_all_fable_out_keeps_the_model() {
    let mut a = [acct(1, 0.5, 0.0, 10.0), acct(2, 0.3, 0.0, 3.0)];
    a[1].tf = 1.5;
    let p = plan(&a, 0.01, 0.0, Some(KAPPA_FABLE));
    let loads = loads_of(&a, &[], None);
    // no target for the Fable pane: it is not sent to run another model
    assert_eq!(pick(&a, &p, &loads, Mover { class: Class::Restricted, rate: 0.01, tokens: None, fable: None }, Some(1), 6.0), None);
    assert_eq!(stranded(&a, Class::Restricted), Some(Stranded::RestrictedOut { back_in_h: 1.5 }));
    assert_eq!(stranded(&a, Class::Plain), None);
}

#[test]
fn allocation_codex_two_accounts() {
    // 2026-11-24 15:00Z: Codex 1 has 0.84 left for 46.6 h, Codex 2 1.00 for 144.8 h; no κ
    let a = [acct(1, 0.84, 0.84, 46.6), acct(2, 1.0, 1.0, 144.8)];
    assert!((a[0].rate_cap() * 100.0 - 1.8).abs() < 0.01);
    assert!((a[1].rate_cap() * 100.0 - 0.69).abs() < 0.01);
    let mut placed: Vec<(Option<u8>, Class, f64)> = Vec::new();
    for _ in 0..4 {
        let p = plan(&a, 0.0, RHO0 * (placed.len() + 1) as f64, None);
        let loads = loads_of(&a, &placed, None);
        let to = pick(&a, &p, &loads, Mover { class: Class::Plain, rate: RHO0, tokens: None, fable: None }, None, HELPER_LAST_H).unwrap();
        placed.push((Some(to), Class::Plain, RHO0));
    }
    let on: Vec<u8> = placed.iter().map(|(o, _, _)| o.unwrap()).collect();
    assert_eq!(on, vec![1, 1, 1, 2], "Codex 1 until its share is met, then Codex 2");
    assert_eq!(old_helper_pick(&a, Class::Plain, &[]), Some(2), "today: the most left");
    // a refused pane on Codex 2 would go to Codex 1 (logged only)
    let p = plan(&a, 0.0, RHO0, None);
    assert_eq!(pick(&a, &p, &loads_of(&a, &[], None), Mover { class: Class::Plain, rate: RHO0, tokens: None, fable: None }, Some(2), 6.0), Some(1));
}

#[test]
fn allocation_speed_rebuild_equals_running_on() {
    let start = 1_795_530_644;
    let mut running = PaneUse::default();
    running.watched(start);
    let mut log = Vec::new();
    for k in 0..(20 * 60) {
        let at = start + k * 60;
        if k % 7 == 0 {
            let (c, amt) = if k % 3 == 0 { (Class::Restricted, 0.0002) } else { (Class::Plain, 0.0001) };
            running.add(at, c, amt);
            log.push((at, c, amt));
        }
        running.prune(at);
    }
    let now = start + 20 * 3600;
    // rebuilt from the last fifteen hours of what was written
    let mut rebuilt = PaneUse::default();
    rebuilt.watched(now - WINDOW_S);
    for (at, c, amt) in log.iter().filter(|(at, _, _)| *at > now - WINDOW_S) {
        rebuilt.add(*at, *c, *amt);
    }
    rebuilt.prune(now);
    for c in [Class::Restricted, Class::Plain] {
        assert_eq!(running.rate(now, c), rebuilt.rate(now, c), "{c:?}");
    }
    assert!(running.rate(now, Class::Plain).unwrap() > 0.0);
}

#[test]
fn allocation_speed_reads_a_steady_rate_as_itself_and_none_until_known() {
    let start = 0;
    let mut u = PaneUse::default();
    u.watched(start);
    assert_eq!(u.rate(start + 60, Class::Plain), None, "not watched long enough");
    for k in 1..=(15 * 60) {
        u.add(start + k * 60, Class::Plain, 0.006 / 60.0);
    }
    let r = u.rate(start + 15 * 3600, Class::Plain).unwrap();
    assert!((r - 0.006).abs() < 0.0002, "{r}");
    assert!(u.rate_or_default(start + 15 * 3600, Class::Plain).2);
    assert_eq!(PaneUse::default().rate_or_default(0, Class::Plain), (0.0, RHO0, false));
    assert_eq!(PaneUse::default().rate_or_default(0, Class::Restricted), (RHO0, 0.0, false), "a Fable pane is taken to use Fable");
}

#[test]
fn allocation_speed_of_a_new_pane_is_not_spread_over_fifteen_hours() {
    let start = 1_795_530_644;
    let mut u = PaneUse::default();
    u.watched(start);
    // two hours watched at a steady 0.01 an hour
    for k in 1..=120 {
        u.add(start + k * 60, Class::Plain, 0.01 / 60.0);
    }
    let r = u.rate(start + 2 * 3600, Class::Plain).unwrap();
    assert!((r - 0.01).abs() < 0.0005, "{r}");
    // minutes watched: averaged over an hour, not over minutes
    let mut v = PaneUse::default();
    v.watched(start);
    v.add(start + 120, Class::Plain, 0.002);
    let r = v.rate(start + 180, Class::Plain).unwrap();
    assert!(r > 0.0022 && r < 0.0023, "{r}");
}

#[test]
fn allocation_restricted_cap_when_the_week_renews_first() {
    // the week renews in 4 h, Fable in 40 h: the week left now only has to last 4 h
    let mut q = acct(1, 0.1, 0.4, 4.0);
    q.tf = 40.0;
    let p = plan(&[q], 1.0, 0.0, Some(KAPPA_FABLE));
    assert!((p.t_f[0] - 0.01).abs() < 1e-12, "min(0.4 / 40, 0.1 / 4): {}", p.t_f[0]);
    let mut q = acct(1, 0.1, 0.4, 4.0);
    q.tf = 8.0;
    let p = plan(&[q], 1.0, 0.0, Some(KAPPA_FABLE));
    assert!((p.t_f[0] - 0.025).abs() < 1e-12, "min(0.4 / 8, 0.1 / 4): {}", p.t_f[0]);
}

#[test]
fn allocation_account_slope_restarts_after_a_drop() {
    let mut s = Slope::default();
    s.note(0, 0.10, 5400);
    assert_eq!(s.rate(), None);
    s.note(1800, 0.13, 5400);
    assert!((s.rate().unwrap() - 0.06).abs() < 1e-12);
    s.note(2400, 0.05, 5400);
    assert_eq!(s.rate(), None, "a reading that went down starts over");
    s.note(3600, 0.07, 5400);
    assert!((s.rate().unwrap() - 0.06).abs() < 1e-9);
    s.note(20_000, 0.08, 5400);
    assert_eq!(s.rate(), None, "older than the window is dropped");
}

fn resting_inputs(accounts: Vec<Quant>, keeps_restricted: bool) -> Inputs {
    let tab = [(Some(1), Class::Plain, 0.01), (Some(1), Class::Plain, 0.02)];
    let loads = loads_of(&accounts, &tab, None);
    let kind = Kind::Rest {
        keeper: false,
        since_move_h: Some(0.25),
        left: vec![(4, 7.5)],
        band_last_hour: 1,
        demand: 0.03,
        stale_feed: false,
        speed_known: true,
        keeps_restricted,
        moves_week: 2,
    };
    Inputs { accounts, b_f: 0.0, b_n: 0.03, kappa: Some(KAPPA_FABLE), loads, mover: Mover { class: Class::Plain, rate: 0.02, tokens: None, fable: None }, here: 1, kind }
}

#[test]
fn allocation_leave_early_skips_stale_and_fable_less_accounts() {
    // P1 lasts under two hours; P2 is stale, P3 has no Fable left, P4 is fresh with Fable
    let mut a = vec![acct(1, 0.03, 0.0, 50.0), acct(2, 0.9, 0.5, 60.0), acct(3, 0.9, 0.0, 70.0), acct(4, 0.5, 0.4, 80.0)];
    a[1].stale = true;
    let go = |a: &[Quant], keeps| {
        let i = resting_inputs(a.to_vec(), keeps);
        i.decide(&i.plan())
    };
    assert_eq!(go(&a, false), Some((3, Why::LeaveEarly)), "the stale P2 lasts longest but is no resting target");
    assert_eq!(go(&a, true), Some((4, Why::LeaveEarly)), "a pane using Fable is not sent where Fable is spent");
    a[3].stale = true;
    assert_eq!(go(&a, true), None);
}

#[test]
fn allocation_replay_reads_back_the_same_inputs_and_answer() {
    let mut a = vec![acct(1, 0.03, 0.0, 50.0), acct(2, 0.9, 0.5, 60.0), acct(3, 0.9, 0.0, 70.0)];
    a[2].tf = 12.345_678_9;
    a[1].open = false;
    let rest = resting_inputs(a.clone(), true);
    let out = Inputs { kind: Kind::Out { horizon_h: HELPER_LAST_H }, kappa: None, ..rest.clone() };
    for i in [rest, out] {
        let line = format!("to=P9 why=x {}", i.encode());
        assert_eq!(Inputs::decode(&line), Some(i.clone()), "{line}");
        assert_eq!(replay(&line), Some(i.decide(&i.plan())));
    }
    assert_eq!(Inputs::decode("here=1 class=plain"), None);
}
