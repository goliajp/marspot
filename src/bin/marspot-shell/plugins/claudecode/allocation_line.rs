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

/// Log `line` under `tag` when `key` differs from the last one, the
/// first time included.
pub(super) fn say_keyed(a: &mut Allocation, log: &mut Vec<(&'static str, String)>, tag: &'static str, key: String, line: String) {
    if a.said.get(tag) == Some(&key) {
        return;
    }
    log.push((tag, line));
    a.said.insert(tag, key);
}

/// What `alloc.plan` is written for: shortage or not, how many panes of
/// each class are on each account, which readings are stale, and where
/// panes would go.  Speeds and targets move with every reply, so they
/// are in the line but do not make one.
pub(super) fn plan_key(accounts: &[Quant], table: &[(Option<u8>, Class, f64)], short: bool, moves: &[Option<String>]) -> String {
    let mut key = String::from(if short { "shortage" } else { "surplus" });
    for q in accounts {
        let on = |c: Class| table.iter().filter(|(a, k, _)| *a == Some(q.id) && *k == c).count();
        key.push_str(&format!(" P{}:{}+{}{}", q.id, on(Class::Restricted), on(Class::Plain), if q.stale { "s" } else { "" }));
    }
    for m in moves.iter().flatten() {
        key.push(' ');
        key.push_str(m);
    }
    key
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

/// The longest `alloc.plan` text, bytes.  The logger caps a whole line
/// at 480 and this tag's prefix takes up to 100 of them; a line cut by
/// the logger lost every account after the cut.
pub(super) const PLAN_MAX: usize = 380;
/// Room kept for the count of what was left out, ` +99`.
const MORE: &str = " +99";

/// How much of an open account's line is written.
#[derive(Clone, Copy)]
enum Detail {
    Full,
    Compact,
    Least,
}

/// One open account: week left, Fable-usable, others-only, hours to
/// renewal, the plan's targets, and the panes on it by class.
fn account_part(q: &Quant, p: &Plan, i: usize, table: &[(Option<u8>, Class, f64)], d: Detail) -> String {
    let on = |c: Class| table.iter().filter(|(a, k, _)| *a == Some(q.id) && *k == c).count();
    let panes = format!("{}+{}{}", on(Class::Restricted), on(Class::Plain), if q.stale { " stale" } else { "" });
    let (r, phi, n) = (q.r * 100.0, q.phi() * 100.0, q.n() * 100.0);
    match d {
        Detail::Full => {
            let (tf, tn) = (pts(p.t_f[i]), pts(p.t_n[i]));
            format!("P{} R{r:.0} Φ{phi:.0} N{n:.0} T{:.0} tF{tf} tN{tn} U{:.0} {panes}", q.id, q.t, p.u[i] * 100.0)
        }
        Detail::Compact => format!("P{} R{r:.0} Φ{phi:.0} N{n:.0} T{:.0} {panes}", q.id, q.t),
        Detail::Least => format!("P{} {r:.0}/{phi:.0}@{:.0} {panes}", q.id, q.t),
    }
}

/// One line under `PLAN_MAX`: the fleet's Fable and other demand
/// against a week's supply, each open account, the accounts out with
/// the hours until each renews, and where panes would go.  Detail is
/// dropped before accounts, and accounts only when the least detail
/// does not fit; what is left out is counted.
pub(super) fn plan_line(accounts: &[Quant], p: &Plan, (b_f, b_n): (f64, f64), table: &[(Option<u8>, Class, f64)], short: bool, moves: &[Option<String>]) -> String {
    let supply: f64 = accounts.iter().map(Quant::rate_cap).sum();
    let week = accounts.len() as f64 / 168.0;
    let mut line = format!(
        "{} A={} B={} week_supply={} short_week={} now_supply={} unassigned_F={}",
        if short { "shortage" } else { "surplus" },
        pts(b_f),
        pts(b_n),
        pts(week),
        u8::from(b_f + b_n > week),
        pts(supply),
        pts(p.unassigned_f)
    );
    let open: Vec<usize> = (0..accounts.len()).filter(|&i| accounts[i].open && accounts[i].r * 100.0 >= 0.5).collect();
    let mut out: Vec<&Quant> = accounts.iter().enumerate().filter(|(i, _)| !open.contains(i)).map(|(_, q)| q).collect();
    out.sort_by(|x, y| x.t.total_cmp(&y.t));
    let out = if out.is_empty() {
        String::new()
    } else {
        let each: Vec<String> = out.iter().map(|q| format!("P{}@{:.0}h", q.id, q.t)).collect();
        format!(" | out={}", each.join(","))
    };
    // at the least detail the accounts share one ` | `, comma separated
    let sep = |d, k: usize| if k > 0 && matches!(d, Detail::Least) { "," } else { " | " };
    let parts = |d| open.iter().enumerate().map(|(k, &i)| format!("{}{}", sep(d, k), account_part(&accounts[i], p, i, table, d))).collect::<Vec<_>>();
    let fits = |ps: &[String], extra: usize| line.len() + ps.iter().map(String::len).sum::<usize>() + out.len() + extra <= PLAN_MAX;
    let mut ps = [Detail::Full, Detail::Compact].into_iter().map(parts).find(|ps| fits(ps, 0)).unwrap_or_else(|| parts(Detail::Least));
    let mut cut = 0;
    while !ps.is_empty() && !fits(&ps, if cut == 0 { 0 } else { MORE.len() }) {
        ps.pop();
        cut += 1;
    }
    ps.iter().for_each(|x| line.push_str(x));
    if cut > 0 {
        line.push_str(&format!(" +{cut}"));
    }
    line.push_str(&out);
    let moves: Vec<&str> = moves.iter().flatten().map(String::as_str).collect();
    if !moves.is_empty() && line.len() + " | would move".len() + MORE.len() <= PLAN_MAX {
        line.push_str(" | would move");
        let mut shown = 0;
        for m in &moves {
            // room left for the count of the ones not shown
            if line.len() + 1 + m.len() + MORE.len() > PLAN_MAX {
                break;
            }
            line.push(' ');
            line.push_str(m);
            shown += 1;
        }
        if shown < moves.len() {
            line.push_str(&format!(" +{}", moves.len() - shown));
        }
    }
    line
}

#[cfg(test)]
#[path = "allocation_line_tests.rs"]
mod tests;
