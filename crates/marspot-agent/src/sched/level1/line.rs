//! The class plan as one log line, amounts in points.

use std::fmt::Write;

use super::{H2, Level1};

fn pts(w: f64) -> String {
    format!("{:.2}", w * 100.0)
}

impl Level1 {
    /// Demand unserved of `class` before `until_h`.
    pub fn unserved_until(&self, class: usize, until_h: f64) -> f64 {
        self.unserved[class].iter().zip(self.cuts.windows(2)).filter(|(_, c)| c[1] <= until_h + 1e-9).map(|(u, _)| u).sum()
    }

    /// `sched.level1`: served and unserved per class (inside the pane
    /// horizon and the week), `pi`, the fluid bound and its parts, the
    /// claims, and per account (ids in `ids`) reserve/spare/prices.
    pub fn line(&self, ids: &[u8]) -> String {
        let mut s = String::new();
        let un = |k: usize, h: f64| pts(self.unserved_until(k, h));
        let m = &self.money;
        let _ = write!(
            s,
            "served={}/{} unserved_h2={}/{} unserved={}/{} pi={:.2} bound={} lapsed={} reserve_taken={} offers_lost={} combos={} tf_early={} claims=",
            pts(self.served[0]),
            pts(self.served[1]),
            un(0, H2),
            un(1, H2),
            un(0, f64::INFINITY),
            un(1, f64::INFINITY),
            self.pi,
            pts(m.total),
            pts(m.lapsed),
            pts(m.reserve_taken),
            pts(m.offers_lost),
            self.combos,
            self.tf_early,
        );
        let claims: Vec<String> = self.claims.iter().map(|c| format!("P{}#{}@{:.1}h:{}", c.account, c.offer, c.at_h, pts(c.value))).collect();
        s.push_str(if claims.is_empty() { "-" } else { "" });
        s.push_str(&claims.join(","));
        for (a, id) in ids.iter().enumerate() {
            let price = self.price[a].map_or_else(|| "-".into(), |p| format!("{:.2}/{:.2}", p[0], p[1]));
            let _ = write!(s, " P{id}={}/{}/{price}", pts(self.reserve[a]), pts(self.spare[a]));
        }
        s
    }
}
