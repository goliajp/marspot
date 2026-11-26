use super::*;
use crate::plugins::claudecode::reset_read::ReadClock;
use marspot::settings::AgentAllocation;
use marspot_agent::sched::reset::READ_EVERY_S;

const NOW: i64 = 1_800_000_000;

/// P2 has the one pane, in `activity`; P3 has none.
fn scan(activity: CcActivity) -> ScanResult {
    let mut r = scan_with(7, 4242, "");
    r.profiles = vec![(2, None), (3, None)];
    r.new_activity.insert(7, activity);
    r
}

#[test]
fn reads_fall_due_per_idle_account_at_most_every_six_hours() {
    let mut clock = ReadClock::default();
    assert_eq!(clock.due(&scan(CcActivity::AwaitingUser), AgentAllocation::Off, NOW), Vec::<u8>::new(), "off: nothing");
    assert_eq!(clock.due(&scan(CcActivity::Working), AgentAllocation::DryRun, NOW), [3], "P2 is at work, P3 has nobody on it");
    assert_eq!(clock.due(&scan(CcActivity::AwaitingUser), AgentAllocation::DryRun, NOW + 60), [2]);
    assert_eq!(clock.due(&scan(CcActivity::AwaitingUser), AgentAllocation::Live, NOW + 3_600), Vec::<u8>::new(), "both read within six hours");
    assert_eq!(clock.due(&scan(CcActivity::AwaitingUser), AgentAllocation::DryRun, NOW + READ_EVERY_S), [3]);
    assert_eq!(clock.due(&scan(CcActivity::Working), AgentAllocation::DryRun, NOW + READ_EVERY_S + 60), Vec::<u8>::new(), "P2 is due but at work");
    assert_eq!(clock.due(&scan(CcActivity::AwaitingUser), AgentAllocation::DryRun, NOW + READ_EVERY_S + 120), [2]);
}

#[test]
fn an_account_gone_from_the_profiles_is_forgotten() {
    let mut clock = ReadClock::default();
    assert_eq!(clock.due(&scan(CcActivity::AwaitingUser), AgentAllocation::DryRun, NOW), [2, 3]);
    let mut only_two = scan(CcActivity::AwaitingUser);
    only_two.profiles = vec![(2, None)];
    clock.due(&only_two, AgentAllocation::DryRun, NOW + 60);
    assert_eq!(clock.due(&scan(CcActivity::AwaitingUser), AgentAllocation::DryRun, NOW + 120), [3], "P3 came back: never read");
}
