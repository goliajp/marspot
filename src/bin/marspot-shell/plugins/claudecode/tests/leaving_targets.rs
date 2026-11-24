//! Where a pane is sent while some account runs short: never onto one
//! that would be left again, and not all onto one that cannot hold
//! them all.

use super::leaving::{account, measured, moved, panes_on, rebalance_mode, write_feed, RENEWS};
use super::*;

/// `quota_home` with a third profile.
fn three(tag: &str) -> TestHome {
    let home = quota_home(tag);
    fs::create_dir_all(home.join(".claude-profile-3/sessions")).unwrap();
    let json = format!("{{\n  \"oauthAccount\": {{\n    \"emailAddress\": \"{tag}3@example\"\n  }}\n}}");
    fs::write(home.join(".claude-profile-3/.claude.json"), json).unwrap();
    home
}

fn mail(tag: &str, n: u8) -> String {
    format!("{tag}{n}@example")
}

/// The profile each moved pane was sent to, by session.
fn targets(host: &FakeHost) -> Vec<(u64, u8)> {
    let mut out: Vec<(u64, u8)> = host
        .sent
        .lock()
        .unwrap()
        .iter()
        .map(|(sid, line)| (*sid, (1..=3).find(|n| line.contains(&format!(".claude-profile-{n}"))).unwrap_or(0)))
        .collect();
    out.sort();
    out
}

/// The allocation would move an idle pane on P2 to P3, which lasts an
/// hour and a half at the speed its readings show: the pane stays
/// where it is.  With P3 lasting, the same pane goes there.  A planned
/// wait (`u_out`) goes though P3 runs short: the core chose that target
/// knowing it would be left again.
#[test]
fn rebalance_an_idle_pane_is_not_moved_onto_an_account_running_short() {
    use crate::plugins::allocation::replay::Why;
    rebalance_mode(marspot::settings::Rebalance::Live);
    for (tag, p3_rising, why, goes) in [("rbs", true, Why::Plan, false), ("rbl", false, Why::Plan, true), ("rbu", true, Why::UOut, true)] {
        let home = three(tag);
        let now = rooms::unix_now();
        write_feed(&home, &[account(&mail(tag, 2), 0.2, RENEWS + 7 * 3600, now), account(&mail(tag, 3), 0.91, RENEWS, now)]);
        let host = FakeHost::new(home.join("state"));
        let (mut plugin, mut result) = panes_on(&home, tag, &host, 2, &[(301, 500.0, false)]);
        let p3 = mail(tag, 3);
        let rising: Vec<(&str, f64)> = if p3_rising { vec![(p3.as_str(), 0.06)] } else { vec![] };
        measured(&mut result, &rising);
        for (s, m) in &plugin.last_meta {
            result.new_meta.insert(*s, m.clone());
        }
        (result.usage, result.choices) = quota_inputs(&result.new_meta);
        let snap = super::super::allocation_inputs::snapshot(&result, &HashMap::new(), &HashMap::new(), now);
        let mut view = super::super::allocation_view::View::of_defaults(&snap);
        let rooms = rooms::rooms_for(&[2, 3], &result.usage.as_ref().unwrap().accounts, |p| Some(mail(tag, p)), now);
        let (_, inputs) = view.by_hand(&rooms, 301, 2, crate::plugins::allocation::Class::Plain).unwrap();
        view.rest.insert(301, (3, why, inputs.expect("the core chose it")));
        result.alloc = Some(view);
        plugin.sweep_unusable_profiles(&host, &result);
        if goes {
            assert_eq!(targets(&host), vec![(301, 3)], "{tag}: {:?}", host.logged.lock().unwrap());
        } else {
            assert!(moved(&host).is_empty(), "{:?}", host.logged.lock().unwrap());
        }
    }
}

/// A refused pane goes to the account that lasts six hours with it,
/// though another renews first; when none lasts, to the one the usual
/// ranking puts first.
#[test]
fn quota_a_refused_pane_prefers_an_account_that_is_not_running_short() {
    rebalance_mode(marspot::settings::Rebalance::DryRun);
    for (tag, p3_rising, to) in [("rfs", false, 3u8), ("rfa", true, 2)] {
        let home = three(tag);
        let now = rooms::unix_now();
        let refused = account(&mail(tag, 1), 1.0, RENEWS, now).replace("\"allowed\"", "\"rejected\"");
        write_feed(&home, &[refused, account(&mail(tag, 2), 0.91, RENEWS, now), account(&mail(tag, 3), if p3_rising { 0.91 } else { 0.2 }, RENEWS + 100, now)]);
        let host = FakeHost::new(home.join("state"));
        let (mut plugin, mut result) = panes_on(&home, tag, &host, 1, &[(311, 500.0, false)]);
        let (p2, p3) = (mail(tag, 2), mail(tag, 3));
        let mut rising = vec![(p2.as_str(), 0.06)];
        if p3_rising {
            rising.push((p3.as_str(), 0.06));
        }
        measured(&mut result, &rising);
        sweep(&mut plugin, &host, &mut result);
        assert_eq!(targets(&host), vec![(311, to)], "{tag}: {:?}", host.logged.lock().unwrap());
    }
}

/// Two panes leave a short P1 in one pass. P2 renews first and lasts
/// six hours with either, not with both: the second goes to P3.
#[test]
fn rebalance_panes_leaving_in_one_pass_do_not_all_land_on_an_account_that_cannot_hold_them() {
    rebalance_mode(marspot::settings::Rebalance::DryRun);
    let tag = "acc";
    let home = three(tag);
    let now = rooms::unix_now();
    write_feed(&home, &[account(&mail(tag, 1), 0.91, RENEWS, now), account(&mail(tag, 2), 0.75, RENEWS + 50, now), account(&mail(tag, 3), 0.2, RENEWS + 100, now)]);
    let host = FakeHost::new(home.join("state"));
    let (mut plugin, mut result) = panes_on(&home, tag, &host, 1, &[(321, 900.0, false), (322, 500.0, false), (323, 100.0, false)]);
    let p1 = mail(tag, 1);
    measured(&mut result, &[(p1.as_str(), 0.06)]);
    sweep(&mut plugin, &host, &mut result);
    let to: Vec<u8> = targets(&host).iter().map(|(_, p)| *p).collect();
    assert_eq!(moved(&host), vec![321, 322], "the slowest stays");
    let mut sorted = to.clone();
    sorted.sort();
    assert_eq!(sorted, vec![2, 3], "one each: {:?}", host.logged.lock().unwrap());
}
