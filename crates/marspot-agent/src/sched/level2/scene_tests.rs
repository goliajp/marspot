//! The P8 scene of 2026-11-27, rebuilt from the readings and the use
//! the records hold for it: what each profile plans for the panes on
//! an account whose other-model room is gone.

use super::tests::{acct, input, pane};
use super::*;

/// 11-27 12:10: P8 at 64% of its week and 28% of Fable, renewing in five
/// days, so all 36 points left are Fable-usable; P1 and P6 renewed full.
/// 488 runs Opus at about 1.3 points an hour with background agents that
/// never rest; 428 is a Fable session whose agents run Opus; 442 Fable.
fn scene() -> (Vec<Account>, Vec<Pane>) {
    let a = vec![acct(8, 0.36, 0.38, 122.0), acct(1, 1.0, 0.528, 168.0), acct(6, 1.0, 0.528, 166.0)];
    let busy = |p: Pane| Pane { long_background: true, clean_per_h: 0.09, lost_work: 0.002, ..p };
    let p = vec![busy(pane(488, 8, false, 0.0, 0.013)), busy(pane(428, 8, true, 0.0106, 0.0079)), pane(442, 8, true, 0.003, 0.0)];
    (a, p)
}

#[test]
fn sched_level2_the_p8_scene_moves_the_opus_and_the_mixed_pane_off_p8() {
    let (a, p) = scene();
    let mut i = input(&a, &p);
    for lim in [Limits::STEADY, Limits::BALANCED, Limits::FULL] {
        i.limits = lim;
        i.pi = lim.pi(0.5);
        let plan = plan(&i, None);
        let of = |sid: u64| plan.switches.iter().find(|s| s.sid == sid && s.order == 0).copied();
        println!(
            "P8 scene {}: 488 {:?}, 428 {:?}, reserve taken {:.2} points (doing nothing {:.2})",
            lim.profile.word(),
            of(488).map(|s| (s.to, s.t_open, s.kind.word())),
            of(428).map(|s| (s.to, s.t_open, s.kind.word())),
            100.0 * plan.totals.reserve_taken,
            100.0 * plan.idle.reserve_taken,
        );
        if lim.profile == Profile::Steady {
            // steady plans the same drain switch, since doing nothing lets
            // other models take 21.8 points of Fable-usable quota, but its
            // executor does not notice a working pane for a switch that
            // averts no refusal within two hours (P8 lasts 7.0): 488's
            // background work never stops, so steady never takes it
            let s = of(488).unwrap_or_else(|| panic!("steady: 488 stays on P8"));
            assert!(!s.exempt && s.kind == Kind::Drain, "steady: {s:?}");
            let w = exec::Watched { open_at: 0, dead_at: (s.t_dead * 3600.0) as i64, exempt: s.exempt, clean_per_h: 0.09, long_background: true, lost_work: 0.002, gain: s.gain, noticed_at: None };
            for t in [0, 3600, (s.t_dead * 3600.0) as i64 - 60] {
                let b = exec::Boundary { at: t, live: true, quiet_s: 600, ..exec::Boundary::default() };
                assert_eq!(exec::decide(&w, &b, &lim), exec::Would::Wait, "steady at {t}");
                let lead = exec::NOTICE_S + i64::from(Limits::BALANCED.notice_wait_min) * 60;
                let balanced = if t >= w.dead_at - lead { exec::Would::Notify(Kind::Drain) } else { exec::Would::Wait };
                assert_eq!(exec::decide(&w, &b, &Limits::BALANCED), balanced, "balanced at {t}");
            }
            continue;
        }
        let s = of(488).unwrap_or_else(|| panic!("{}: 488 stays on P8", lim.profile.word()));
        assert!(matches!(s.to, 1 | 6) && s.t_open <= 2.0 && s.kind == Kind::Drain, "{}: {s:?}", lim.profile.word());
        let m = of(428).unwrap_or_else(|| panic!("{}: 428 stays on P8", lim.profile.word()));
        assert!(matches!(m.to, 1 | 6), "{}: {m:?}", lim.profile.word());
    }
}
