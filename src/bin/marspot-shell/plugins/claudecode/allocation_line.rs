//! The allocation's log lines, written when what they say changes.

use super::allocation_adapter::Allocation;
use super::allocation_inputs::Snapshot;
use crate::plugins::allocation::plan::Plan;
use crate::plugins::allocation::{Class, KAPPA_FABLE, Quant};

/// A κ bound this far under the constant is worth a line.
const KAPPA_SLACK: f64 = 0.05;

/// Log `line` under `tag` when it differs from the last one; the first
/// one only when `first_too`, so a state that starts out fine is quiet.
pub(super) fn say(a: &mut Allocation, log: &mut Vec<(&'static str, String)>, tag: &'static str, line: String, first_too: bool) {
    match a.said.get(tag) {
        Some(prev) if *prev == line => return,
        None if !first_too => {}
        _ => log.push((tag, line.clone())),
    }
    a.said.insert(tag, line);
}

/// The κ the readings allow: an account's week has gone up at least κ
/// times what its Fable cap has.  Only noted, never adopted.
pub(super) fn kappa_check(a: &mut Allocation, snap: &Snapshot, log: &mut Vec<(&'static str, String)>) {
    let bound = snap
        .accounts
        .iter()
        .filter_map(|ac| ac.fable_used.filter(|f| *f >= 0.5).map(|f| (ac.profile, ac.week_used / f)))
        .min_by(|x, y| x.1.total_cmp(&y.1));
    let line = match bound {
        Some((p, b)) if b < KAPPA_FABLE - KAPPA_SLACK => format!("P{p}'s readings allow κ ≤ {b:.2}, under {KAPPA_FABLE}; kept {KAPPA_FABLE}"),
        _ => format!("readings agree with κ = {KAPPA_FABLE}"),
    };
    let off = bound.is_some_and(|(_, b)| b < KAPPA_FABLE - KAPPA_SLACK);
    say(a, log, "alloc.kappa_check", line, off);
}

fn pts(x: f64) -> String {
    format!("{:.1}", x * 100.0)
}

/// One line: shortage, each account's quantities and targets, the
/// panes on it, and where panes would go.
pub(super) fn plan_line(accounts: &[Quant], p: &Plan, table: &[(Option<u8>, Class, f64)], short: bool, moves: &[Option<String>]) -> String {
    let supply: f64 = accounts.iter().map(Quant::rate_cap).sum();
    let demand: f64 = table.iter().map(|(_, _, r)| r).sum();
    let mut line = format!("{} demand={} supply={} pts/h", if short { "shortage" } else { "surplus" }, pts(demand), pts(supply));
    for (i, q) in accounts.iter().enumerate() {
        let on = |c: Class| table.iter().filter(|(a, k, _)| *a == Some(q.id) && *k == c).count();
        line.push_str(&format!(
            " | P{} R={:.0} Φ={:.0} N={:.0} T={:.0} tF={} tN={} U={:.0} panes={}+{}{}",
            q.id,
            q.r * 100.0,
            q.phi() * 100.0,
            q.n() * 100.0,
            q.t,
            pts(p.t_f[i]),
            pts(p.t_n[i]),
            p.u[i] * 100.0,
            on(Class::Restricted),
            on(Class::Plain),
            if q.stale { " stale" } else { "" }
        ));
    }
    if p.unassigned_f > 0.0 {
        line.push_str(&format!(" | Fable unassigned={}", pts(p.unassigned_f)));
    }
    let moves: Vec<&str> = moves.iter().flatten().map(String::as_str).collect();
    if !moves.is_empty() {
        line.push_str(&format!(" | would move {}", moves.join(" ")));
    }
    line
}
