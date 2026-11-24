//! The simulator's pass conditions, as they stand.
//!
//! The verdict of every condition on every row is written down below:
//! a change in the rules or the simulator that turns any of them, to
//! pass or to fail, fails this test until the table is brought up to
//! date.  The table also says why the new rule moves nothing yet.

use super::sim::{Case, run};
use super::sim_gate::{CONDITIONS, line, mean, sigma, step4_gate};
use super::sim_policies::Policy;

/// Per row, conditions 1 to 5 (see `sim_gate`): `None` where a
/// condition is not set for the row.
const EXPECTED: [(&str, [Option<bool>; CONDITIONS]); 10] = [
    ("7 pts/h, same speeds", [Some(true), None, Some(true), Some(true), Some(false)]),
    ("7 pts/h, speeds differ", [Some(true), None, Some(false), Some(true), Some(false)]),
    ("believed x0.5", [Some(true), None, None, Some(false), Some(false)]),
    ("believed x2", [Some(false), None, None, Some(true), Some(false)]),
    ("0.136 pts a move", [Some(true), Some(true), Some(false), Some(true), Some(false)]),
    ("0.5 pts a move", [Some(true), None, Some(false), Some(true), Some(true)]),
    ("helper batches", [None, None, Some(true), Some(true), Some(false)]),
    ("three panes stop at 72 h", [None, None, Some(false), Some(false), Some(false)]),
    ("4 pts/h", [None, None, None, Some(true), Some(true)]),
    ("2 pts/h, speeds differ", [None, None, None, Some(true), Some(true)]),
];

#[test]
fn allocation_sim_step4_gate() {
    let gate = step4_gate();
    eprintln!("conditions: 1 refusals <= old/3 | 2 unmet <= old at 0.136 pts a move | 3 waste <= 0.01 | 4 every pane <= 6 resting moves | 5 Fable unmet <= old");
    for r in &gate.rows {
        eprintln!("{}", line(r));
    }
    eprintln!("step 4 gate open: {}", gate.open);
    let got: Vec<(&str, [Option<bool>; CONDITIONS])> = gate.rows.iter().map(|r| (r.name, r.holds)).collect();
    assert_eq!(got, EXPECTED.to_vec(), "a verdict changed:\n{got:#?}");
    assert!(!gate.open, "every condition holds: the table above is out of date");
}

#[test]
fn allocation_sim_surplus_rows() {
    let mut four = Case::base(2, 9, 0.04);
    four.fable_share = 2.0 / 11.0;
    let mut two = sigma(Case::base(2, 4, 0.02));
    two.fable_share = 1.0 / 3.0;
    for (name, c) in [("4 pts/h", four), ("2 pts/h, speeds differ", two)] {
        let (old, new) = (mean(Policy::Old, &c), mean(Policy::Plan, &c));
        assert!(new.refusals <= old.refusals, "{name}: {new:?} vs {old:?}");
        assert!(new.unmet <= old.unmet + 0.5, "{name}: {new:?} vs {old:?}");
    }
}

/// At the κ of the first measurements, three panes leaving at 72 h
/// leave the Fable panes short in the last day: the plain panes have
/// used what Fable could have, and the new rule does far worse than
/// today's.
#[test]
fn allocation_sim_three_panes_stop_at_kappa_0_57() {
    let mut c = sigma(Case::base(3, 9, 0.07));
    c.leave = Some((3, 72.0));
    c.kappa = 0.57;
    let (old, new) = (mean(Policy::Old, &c), mean(Policy::Plan, &c));
    eprintln!("kappa 0.57, three panes stop at 72 h: old {old:?}\nnew {new:?}");
    let near = |x: f64, y: f64| (x - y).abs() < 0.1;
    assert!(near(old.unmet, 8.5) && near(old.unmet_restricted, 5.2), "{old:?}");
    assert!(near(new.unmet, 36.1) && near(new.unmet_restricted, 35.7), "{new:?}");
}

#[test]
fn allocation_sim_is_deterministic() {
    let c = sigma(Case::base(3, 9, 0.07));
    assert_eq!(run(Policy::Plan, &c, 5), run(Policy::Plan, &c, 5));
}
