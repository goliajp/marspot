//! The closed loop gives the same bits on every machine.
//!
//! A digest of every week's outcome (each money part's bits, every
//! count) over runs of both policies, the planner driven both ways.  The
//! digest is the one recorded here on every machine: it changes with the
//! planner, and is then recorded again, but a machine that reads another
//! than the rest has a decision that rests on its platform (a libm call,
//! an unordered sum, a hash map's order) and the acceptance numbers it
//! gives are not the code's.

use super::loop_world::{Policy, Week, run};

/// The digest of `RUNS`, recorded.
const RECORDED: u64 = 0xd3aa_f6ea_84d1_c20f;

/// Seeds and policies: the planner with the forecast right and today's
/// rule on four weeks; driven as the shell with the forecast three times
/// low and its three-hour speed as wrong, the run whose refusals read
/// 152 on one machine and 178 on another, on one.
pub(super) const RUNS: [(u64, u64, Policy); 3] = [
    (1000, 1004, Policy::Today { bias: 1.0 }),
    (1000, 1004, Policy::Planner { bias: 1.0, short: 1.0, shell: false }),
    (1000, 1001, Policy::Planner { bias: 1.0 / 3.0, short: 1.0 / 3.0, shell: true }),
];

pub(super) struct Fnv(pub u64);

impl Fnv {
    pub(super) fn eat(&mut self, x: u64) {
        for b in x.to_le_bytes() {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    pub(super) fn week(&mut self, w: &Week) {
        for x in [w.money, w.unmet, w.switch_rw, w.refusal_rw, w.lapsed] {
            self.eat(x.to_bits());
        }
        for n in [w.refusals, w.switches, w.double, w.switches_r, w.refusals_r] {
            self.eat(n as u64);
        }
    }
}

#[test]
fn sched_level2_closed_loop_digest_is_the_same_on_every_machine() {
    let mut all = Fnv(0xcbf2_9ce4_8422_2325);
    for (from, to, policy) in RUNS {
        let weeks: Vec<Week> = std::thread::scope(|s| {
            let h: Vec<_> = (from..to).map(|k| s.spawn(move || run(k, policy))).collect();
            h.into_iter().map(|h| h.join().expect("a week ran")).collect()
        });
        let mut one = Fnv(0xcbf2_9ce4_8422_2325);
        for w in &weeks {
            one.week(w);
            all.week(w);
        }
        let refusals: usize = weeks.iter().map(|w| w.refusals).sum();
        let money: f64 = weeks.iter().map(|w| w.money).sum();
        println!("closed loop digest {policy:?} seeds {from}..{to}: {:016x} (refusals {refusals}, money {:.4} points)", one.0, 100.0 * money);
    }
    println!("closed loop digest: {:016x}", all.0);
    assert_eq!(all.0, RECORDED, "the closed loop read {:016x}, recorded {RECORDED:016x}", all.0);
}
