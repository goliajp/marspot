//! Every logged account decision, weighed again by the selector.
//!
//! The rows are the `would_move`, `moved` and `placed` lines of
//! 2026-11-24 to 11-26, each with every input its decision read.  The
//! objective in force decides each row's inputs again, and the
//! selector, given the candidates and terms `sched::alloc` builds from
//! the same inputs, must decide the same: the account, or staying.
//!
//! The assertion covers waste-rule decisions only: a row logged under
//! the bands is decided again by the objective (with no resting moves
//! counted that day), never by the bands, as the shell's
//! `svc.sched.decide` line weighs it (`rule=bands sel_rule=waste`).
//!
//! What the rows logged was decided by the rules of their day (the
//! bands for lines with no `rule=` word, and a leave-early move ranked
//! among the accounts it allowed); those decisions are counted against
//! the selector too and printed, not asserted.

use marspot_agent::quota::replay::{Inputs, Rule};
use marspot_agent::sched::{POLICY, alloc};

const ROWS: &str = include_str!("fixtures/alloc_decisions_20261126.tsv");

struct Row<'a> {
    sid: &'a str,
    kind: &'a str,
    logged: Option<u8>,
    detail: &'a str,
}

fn rows() -> Vec<Row<'static>> {
    ROWS.lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let to = f[5].split(' ').find_map(|w| w.strip_prefix("to=P")).and_then(|p| p.parse().ok());
            let logged = if f[2] == "would_move" { to } else { f[3].parse().ok() };
            Row { sid: f[1], kind: f[2], logged, detail: f[5] }
        })
        .collect()
}

fn in_force(line: &str) -> Rule {
    match Rule::of_line(line) {
        r @ Rule::Waste { .. } => r,
        Rule::Bands => Rule::waste(0),
    }
}

fn word(to: Option<u8>) -> String {
    to.map_or_else(|| "stay".into(), |t| format!("P{t}"))
}

#[test]
fn sched_every_existing_decision_equals_choose() {
    let rows = rows();
    let count = |k: &str| rows.iter().filter(|r| r.kind == k).count();
    assert_eq!((count("would_move"), count("moved"), count("placed")), (72, 18, 7), "the fixture read wrong");
    let (mut same, mut differ) = (0, Vec::new());
    let (mut logged_same, mut logged_differ) = (0, Vec::new());
    let mut line_rule_same = 0;
    for (n, r) in rows.iter().enumerate() {
        let i = Inputs::decode(r.detail).unwrap_or_else(|| panic!("row {n} does not decode: {}", r.detail));
        let rule = in_force(r.detail);
        let existing = i.decide_by(&i.plan(), rule).map(|x| x.0);
        let chosen = alloc::decide(&i, rule, &|_| true, 0.0, &POLICY);
        if chosen.action == existing {
            same += 1;
        } else {
            differ.push(format!("row {n} pane {} {}: objective {} selector {} ({:?})", r.sid, r.kind, word(existing), word(chosen.action), chosen));
        }
        if chosen.action == r.logged {
            logged_same += 1;
        } else {
            logged_differ.push(format!("row {n} pane {} {}: logged {} selector {}", r.sid, r.kind, word(r.logged), word(chosen.action)));
        }
        line_rule_same += usize::from(i.decide_by(&i.plan(), Rule::of_line(r.detail)).map(|x| x.0) == r.logged);
    }
    println!("objective in force against the selector: {same} equal, {} differ", differ.len());
    println!("logged against the selector: {logged_same} equal, {} differ", logged_differ.len());
    println!("logged against its own line's rule: {line_rule_same} equal of {}", rows.len());
    for d in &logged_differ {
        println!("  {d}");
    }
    assert!(differ.is_empty(), "the selector decides otherwise:\n{}", differ.join("\n"));
}
