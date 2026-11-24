//! The hook budget: what strikes a plugin and what does not.

use super::*;

/// A hook that is slow on the clock but used no CPU was waiting,
/// not working. Striking it would disable a plugin over somebody
/// else's load -- and the plugin that gets disabled is the one
/// that reclaims panes and switches accounts.
#[test]
fn being_descheduled_is_not_an_overshoot() {
    let mut slot = test_slot();
    for _ in 0..(BUDGET_OVERSHOOT_LIMIT + 2) {
        check_budget(&mut slot, "tick", HOOK_BUDGET * 10, Duration::from_micros(200));
    }
    assert!(slot.enabled, "a plugin must not be disabled for waiting");
    assert_eq!(slot.consecutive_overshoots, 0);
}

#[test]
fn burning_the_thread_still_strikes() {
    let mut slot = test_slot();
    for _ in 0..BUDGET_OVERSHOOT_LIMIT {
        check_budget(&mut slot, "tick", HOOK_BUDGET * 10, HOOK_BUDGET * 2);
    }
    assert!(!slot.enabled, "sustained CPU over budget is what this is for");
}

#[test]
fn one_clean_run_clears_the_strikes() {
    let mut slot = test_slot();
    check_budget(&mut slot, "tick", HOOK_BUDGET * 10, HOOK_BUDGET * 2);
    check_budget(&mut slot, "tick", Duration::from_millis(1), Duration::from_micros(50));
    assert_eq!(slot.consecutive_overshoots, 0);
    check_budget(&mut slot, "tick", HOOK_BUDGET * 10, HOOK_BUDGET * 2);
    check_budget(&mut slot, "tick", HOOK_BUDGET * 10, HOOK_BUDGET * 2);
    assert!(slot.enabled, "the strikes have to be consecutive");
}

/// The clock has to move, or every hook looks free and the budget
/// stops meaning anything.
#[test]
fn the_cpu_clock_actually_advances() {
    let before = thread_cpu_now();
    let mut n: u64 = 0;
    for i in 0..3_000_000u64 {
        n = n.wrapping_add(i).rotate_left(7);
    }
    assert_ne!(n, 0);
    assert!(
        thread_cpu_now() > before,
        "CLOCK_THREAD_CPUTIME_ID did not move across three million rotations"
    );
}

fn test_slot() -> Slot {
    Slot {
        plugin: Box::new(Nothing),
        metadata: PluginMetadata {
            name: "t",
            api_version: PLUGIN_API_VERSION,
            tick_interval_ms: 1000,
        },
        enabled: true,
        consecutive_overshoots: 0,
        last_tick: Instant::now(),
    }
}

struct Nothing;
impl Plugin for Nothing {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "t",
            api_version: PLUGIN_API_VERSION,
            tick_interval_ms: 1000,
        }
    }
}
