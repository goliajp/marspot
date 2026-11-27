use super::*;
use super::super::allocation_inputs::Snapshot;
use marspot_agent::sched::reset::Supply;

const NOW: i64 = 1_795_600_000;

fn account(profile: u8, week_used: f64, supply: Vec<Supply>) -> AccountIn {
    AccountIn {
        profile,
        week_used,
        fable_used: Some(0.9),
        fable_known: true,
        week_renews: NOW + 58 * 3600,
        fable_renews: NOW + 58 * 3600,
        open: true,
        measured_at: NOW - 60,
        supply,
    }
}

fn view(accounts: &[AccountIn], b_f: f64, b_n: f64) -> View {
    let snap = Snapshot { now: NOW, accounts: accounts.to_vec(), ..Snapshot::default() };
    let mut v = View::from_rates(&snap, &[], 0.0);
    (v.b_f, v.b_n) = (b_f, b_n);
    v
}

fn window_reset() -> Supply {
    Supply { week: false, window: true, gives_back_week: true, moves_week: false, requires_limit: true, starts_h: 0.0, ends_h: None, left: 1, usable_below_left: None }
}

#[test]
fn allocation_level1_inputs_read_accounts_without_resets_and_round_the_scan_rates() {
    let a = [account(3, 0.93, vec![window_reset()]), account(7, 0.5, Vec::new())];
    let v = view(&a, 0.01234, 0.03456);
    let i = inputs(&a, &v, None, 0.0, NOW);
    // the reset is an offer, not counted into the reading
    assert!((i.accounts[0].r - 0.07).abs() < 1e-9, "{:?}", i.accounts[0]);
    assert!((i.accounts[0].f - KAPPA_FABLE * 0.1).abs() < 1e-9);
    assert_eq!(i.offers, vec![Offer { account: 3, supply: window_reset() }]);
    assert!((i.demand[0] - 0.012).abs() < 1e-12 && (i.demand[1] - 0.035).abs() < 1e-12, "{:?}", i.demand);
    // no forecast: the reserve at the level; the readings' slope floors Fable
    assert_eq!(i.prudent, i.demand[0]);
    assert!(i.buckets.is_empty());
    let i = inputs(&a, &v, None, 0.02, NOW);
    assert_eq!(i.demand[0], 0.02);
}

#[test]
fn allocation_level1_is_worked_out_when_its_inputs_change_and_logged_when_its_line_does() {
    let mut a = vec![account(3, 0.93, Vec::new()), account(7, 0.5, Vec::new())];
    let mut p = ClassPlan::default();
    let line = p.scan(&a, &view(&a, 0.012, 0.03), None, 0.0, NOW).expect("the first plan is logged");
    assert!(line.starts_with("served=") && line.contains(" P3=") && line.contains(" P7="), "{line}");
    // the same inputs a scan later: nothing worked out, nothing logged
    assert_eq!(p.scan(&a, &view(&a, 0.012, 0.03), None, 0.0, NOW + 2), None);
    // scan rates that round the same: nothing
    assert_eq!(p.scan(&a, &view(&a, 0.0121, 0.0301), None, 0.0, NOW + 4), None);
    // a new reading: a new plan and a new line
    a[0].week_used = 0.95;
    a[0].measured_at = NOW;
    let again = p.scan(&a, &view(&a, 0.012, 0.03), None, 0.0, NOW + 6).expect("a new reading changes the line");
    assert_ne!(again, line);
    // no account, no plan
    assert_eq!(ClassPlan::default().scan(&[], &view(&[], 0.01, 0.01), None, 0.0, NOW), None);
}
