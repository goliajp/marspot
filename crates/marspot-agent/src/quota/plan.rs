//! The allocation: by renewal order, load each account with what it
//! can use up exactly by the time it renews.
//!
//! An account carrying `R / T` an hour runs out the moment it renews.
//! The restricted demand is placed first, an hour at a time for a week
//! ahead, earliest restricted renewal first, so what it will take out
//! of each account before that account's week renews (`D_F`) is known
//! and kept back from the other models.  Those then get, earliest week
//! renewal first, up to `U / T` each, `U = max(R − D_F, N)`; what is
//! left once every account is at its cap is spread in proportion to
//! `U`, not piled onto one account, which would run out in hours and
//! refuse every pane on it at once.  Restricted demand no account can
//! take is left unassigned.

use super::{EPS, HORIZON_H, Quant};

/// Each account's targets for the coming hour, in the caller's order.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// Restricted speed this account should carry now.
    pub t_f: Vec<f64>,
    /// Plain speed this account should carry now.
    pub t_n: Vec<f64>,
    /// What the other models can use here before the week renews.
    pub u: Vec<f64>,
    /// What the restricted model is expected to take before then.
    pub d_f: Vec<f64>,
    /// Restricted demand of the first hour no account could take.
    pub unassigned_f: f64,
    /// Plain demand of the first hour beyond every account's cap,
    /// spread in proportion to `U`.
    pub overflow_n: f64,
}

/// `b_f`, `b_n`: restricted and plain demand an hour.  `kappa`: the
/// restricted window's share of a week; `None` reserves nothing.
pub fn plan(accounts: &[Quant], b_f: f64, b_n: f64, kappa: Option<f64>) -> Plan {
    let (t_f, d_f, unassigned_f) = restricted_pass(accounts, b_f, kappa);
    let u: Vec<f64> = accounts
        .iter()
        .zip(&d_f)
        .map(|(q, d)| if q.r > 0.0 { (q.r - d).max(q.n()) } else { 0.0 })
        .collect();
    let mut t_n = vec![0.0; accounts.len()];
    let mut need = b_n;
    for i in by_renewal(accounts, |i| u[i] > EPS, |q| q.t) {
        let x = (u[i] / accounts[i].t.max(0.5)).min(need);
        t_n[i] = x;
        need -= x;
    }
    let overflow_n = if need > 1e-12 { need } else { 0.0 };
    let total_u: f64 = u.iter().sum();
    if overflow_n > 0.0 && total_u > 0.0 {
        for (t, ui) in t_n.iter_mut().zip(&u) {
            *t += overflow_n * ui / total_u;
        }
    }
    Plan { t_f, t_n, u, d_f, unassigned_f, overflow_n }
}

/// Indices of the accounts that pass `keep`, earliest `key` first.
fn by_renewal(accounts: &[Quant], keep: impl Fn(usize) -> bool, key: impl Fn(&Quant) -> f64) -> Vec<usize> {
    let mut order: Vec<usize> = (0..accounts.len()).filter(|&i| keep(i)).collect();
    order.sort_by(|&a, &b| key(&accounts[a]).total_cmp(&key(&accounts[b])).then(accounts[a].id.cmp(&accounts[b].id)));
    order
}

#[derive(Clone)]
struct Ahead {
    r: f64,
    f: f64,
    t: f64,
    tf: f64,
    renewed: bool,
}

/// The hourly forward pass for the restricted model: first-hour
/// targets, what it takes before each week renews, and what of the
/// first hour nobody could take.
///
/// An account's restricted cap is `f / T_F`, but never more of the week
/// than `R / min(T, T_F)`: when the week renews first, the week left
/// now only has to last until then.
fn restricted_pass(accounts: &[Quant], b_f: f64, kappa: Option<f64>) -> (Vec<f64>, Vec<f64>, f64) {
    let n = accounts.len();
    let mut s: Vec<Ahead> = accounts.iter().map(|q| Ahead { r: q.r, f: q.f, t: q.t, tf: q.tf, renewed: false }).collect();
    let (mut t_f, mut d_f) = (vec![0.0; n], vec![0.0; n]);
    let mut unassigned = 0.0;
    for h in 0..HORIZON_H {
        let mut order: Vec<usize> = (0..n).filter(|&i| s[i].f.min(s[i].r) > EPS).collect();
        order.sort_by(|&a, &b| s[a].tf.total_cmp(&s[b].tf).then(accounts[a].id.cmp(&accounts[b].id)));
        let mut need = b_f;
        for i in order {
            if need <= 1e-12 {
                break;
            }
            // what lasts until the restricted window renews, and no more of
            // the week than lasts until whichever renews first
            let cap = (s[i].f / s[i].tf.max(0.5)).min(s[i].r / s[i].t.min(s[i].tf).max(0.5));
            let x = cap.min(need);
            s[i].f -= x;
            s[i].r -= x;
            need -= x;
            if !s[i].renewed && kappa.is_some() {
                d_f[i] += x;
            }
            if h == 0 {
                t_f[i] = x;
            }
        }
        if h == 0 && need > 1e-12 {
            unassigned = need;
        }
        for a in &mut s {
            a.t -= 1.0;
            a.tf -= 1.0;
            if a.t <= 0.0 {
                a.r = 1.0;
                a.t += 168.0;
                a.renewed = true;
            }
            if a.tf <= 0.0 {
                a.f = kappa.unwrap_or(1.0);
                a.tf += 168.0;
            }
        }
    }
    (t_f, d_f, unassigned)
}
