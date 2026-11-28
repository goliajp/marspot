//! Placement while the fleet is short.  When demand over the pane
//! horizon exceeds what the accounts hold, the plan's panes take every
//! account's room and no entry holds a newcomer; the table still places
//! it.  It goes to today's rule's account when the caller names one it
//! may go to: on the recorded week that rule's pick, which reads both
//! windows, lapsed no more Fable than the table's own pricing.  Else it
//! goes where it costs least: the account with the most room left after
//! the plan and the newcomer's expected use, so the least of its use goes
//! unserved.  The room counts the plan's switches onto the account later
//! in the day, which the account's load right now does not.  Protection
//! still holds: under it a plain newcomer never goes to an account whose
//! week is all Fable-usable, today's rule's or not, and gets nothing when
//! only such accounts are left.

use super::{Class, EPS, Newcomer, Room, Routing};

impl Room {
    /// Room left after the plan, below zero when the plan's panes
    /// need more than the account holds before it renews.
    pub(super) fn room(&self, class: Class) -> f64 {
        match class {
            Class::Restricted => self.window.min(self.week),
            Class::Plain => self.plain.min(self.week),
        }
    }

    fn cap(&self, class: Class) -> f64 {
        self.cap[usize::from(class == Class::Plain)]
    }
}

impl Routing {
    /// Protection keeps a newcomer of `class` off `r`.
    pub(super) fn kept_off(&self, r: &Room, class: Class) -> bool {
        self.protect && class == Class::Plain && !r.plain_only
    }

    /// The accounts with something for `class` but no room left after
    /// the plan, the most room first.
    pub(super) fn short_order(&self, class: Class) -> Vec<u8> {
        self.by_cost(class, |r| r.room(class), |r| r.left(class) <= EPS)
    }

    /// Where `m` goes when no entry holds it, among every account with
    /// something for its class that `ok` allows: `today` when it is one
    /// of them, else where it costs least.
    pub(super) fn least_bad(&self, m: &Newcomer, ok: impl Fn(u8) -> bool, today: Option<u8>) -> Option<u8> {
        let v = self.by_cost(m.class, |r| r.room(m.class) - m.usage(r.renews(m.class)), |r| ok(r.id));
        today.filter(|t| v.contains(t)).or_else(|| v.first().copied())
    }

    /// The accounts `keep` allows with something for `class` that
    /// protection leaves it, the highest `after` first.
    fn by_cost(&self, class: Class, after: impl Fn(&Room) -> f64, keep: impl Fn(&Room) -> bool) -> Vec<u8> {
        let mut v: Vec<(f64, u8)> = self.rooms.iter().filter(|r| r.cap(class) > EPS && !self.kept_off(r, class) && keep(r)).map(|r| (after(r), r.id)).collect();
        v.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        v.iter().map(|x| x.1).collect()
    }

    /// Protection leaves a newcomer of `class` nowhere: every account
    /// with week left has only Fable-usable week.
    pub fn kept_off_all(&self, class: Class) -> bool {
        !self.rooms.is_empty() && self.rooms.iter().all(|r| self.kept_off(r, class))
    }

    /// One line, in points: each class's entries in order with their room,
    /// then those without room, below zero, then the other agents.
    pub fn line(&self) -> String {
        let mut s = String::new();
        for (word, class) in [("fable", Class::Restricted), ("plain", Class::Plain)] {
            let mut ids = self.order(class);
            ids.extend(self.short_order(class));
            let entries: Vec<String> = ids.iter().filter_map(|id| self.rooms.iter().find(|r| r.id == *id)).map(|r| format!("P{}={:.1}", r.id, 100.0 * r.room(class))).collect();
            s.push_str(&format!("{word}={} ", if entries.is_empty() { "-".into() } else { entries.join(",") }));
        }
        let others: Vec<String> = self.others.iter().map(|(a, l)| format!("{a}={:.1}", 100.0 * l)).collect();
        s.push_str(&format!("others={}", if others.is_empty() { "-".into() } else { others.join(",") }));
        s
    }
}
