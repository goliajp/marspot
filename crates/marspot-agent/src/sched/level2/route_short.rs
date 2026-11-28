//! Placement while the fleet is short.  When demand over the pane
//! horizon exceeds what the accounts hold, the plan's panes take every
//! account's room and no entry holds a newcomer; the table still places
//! it.  It goes to today's rule's account when the caller names one it
//! may go to: on the recorded week that rule's pick, which reads both
//! windows, lapsed no more Fable than the table's own pricing.  When
//! today's account is one the table does not hold (its input read it
//! closed, stale or empty) the table places nothing and today's rule
//! does: on the recorded week sending those newcomers where the table's
//! pricing said took 9.9 points more of `Phi` under full.  Else it
//! goes where it costs least: the account with the most room left after
//! the plan and the newcomer's expected use, so the least of its use goes
//! unserved.  The room counts the plan's switches onto the account later
//! in the day, which the account's load right now does not.  Protection
//! still holds: under it a plain newcomer never goes to an account whose
//! week is all Fable-usable, today's rule's or not, while another account
//! has week beyond its Fable-usable quota.  When none has, other models
//! keep working: the newcomer goes among the Fable-usable accounts the
//! same way, marked `fable_only`.  A refused pane nobody is at that
//! strict protection holds waits instead (`place_refused`, `guard`).

use super::{Class, EPS, Newcomer, RefusedTo, Room, Routing};

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
    /// Where refused pane `sid` leaving `from` goes: as `place`, except
    /// that it goes short only to `today`'s pick, never when today's rule
    /// has none, and that under protection a pane the plan knows nobody
    /// is at skips every account an attended pane is on, never goes
    /// short, and waits when no entry holds it.
    pub fn place_refused(&mut self, m: &Newcomer, from: u8, sid: u64, today: Option<u8>) -> RefusedTo {
        if !(self.keep_attended && self.unattended.contains(&sid)) {
            let short = today.is_some().then_some(today);
            return self.place_where(m, Some(from), |_| true, short).map_or(RefusedTo::Nowhere, RefusedTo::To);
        }
        let held: Vec<u8> = self.rooms.iter().filter(|r| r.attended).map(|r| r.id).collect();
        self.place_where(m, Some(from), |id| !held.contains(&id), None).map_or(RefusedTo::Wait, RefusedTo::To)
    }

    /// Protection keeps a newcomer of `class` off `r`.
    pub(super) fn kept_off(&self, r: &Room, class: Class) -> bool {
        self.protect && class == Class::Plain && !r.plain_only
    }

    /// The accounts with something for `class` that `order` leaves out,
    /// the most room first: those without room left after the plan, and
    /// under protection the Fable-usable ones when nothing else is left.
    fn short_order(&self, class: Class) -> Vec<u8> {
        let held = self.order(class);
        self.by_cost(class, |r| r.room(class), |r| !held.contains(&r.id))
    }

    /// Where `m` goes when no entry holds it, among every account with
    /// something for its class that `ok` allows: `today` when it is one
    /// of them, else where it costs least.  Nowhere when `today` is an
    /// account the table does not hold at all: the plan's input read it
    /// closed, stale or empty, and today's rule, which read it fresher,
    /// is left to place the newcomer there.
    pub(super) fn least_bad(&self, m: &Newcomer, ok: impl Fn(u8) -> bool, today: Option<u8>) -> Option<u8> {
        if today.is_some_and(|t| !self.rooms.iter().any(|r| r.id == t)) {
            return None;
        }
        let v = self.by_cost(m.class, |r| r.room(m.class) - m.usage(r.renews(m.class)), |r| ok(r.id));
        today.filter(|t| v.contains(t)).or_else(|| v.first().copied())
    }

    /// The accounts `keep` allows with something for `class` that
    /// protection leaves it, or every one when it leaves none, the
    /// highest `after` first.
    fn by_cost(&self, class: Class, after: impl Fn(&Room) -> f64, keep: impl Fn(&Room) -> bool) -> Vec<u8> {
        let all_off = self.kept_off_all(class);
        let mut v: Vec<(f64, u8)> = self.rooms.iter().filter(|r| r.cap(class) > EPS && (all_off || !self.kept_off(r, class)) && keep(r)).map(|r| (after(r), r.id)).collect();
        v.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        v.iter().map(|x| x.1).collect()
    }

    /// Protection leaves a newcomer of `class` nowhere: every account
    /// with week left has only Fable-usable week.
    pub fn kept_off_all(&self, class: Class) -> bool {
        !self.rooms.is_empty() && self.rooms.iter().all(|r| self.kept_off(r, class))
    }

    /// An account a plain newcomer could wrongly be placed on: one whose
    /// week is all Fable-usable but holds something for the plain class,
    /// with room after the plan or not.
    pub fn fable_only_room(&self) -> bool {
        self.rooms.iter().any(|r| !r.plain_only && r.cap(Class::Plain) > EPS)
    }

    /// The accounts in the line's order for each class, restricted then
    /// plain, without their room: what a probe's count is keyed on.
    pub fn order_ids(&self) -> [Vec<u8>; 2] {
        [Class::Restricted, Class::Plain].map(|c| {
            let mut ids = self.order(c);
            ids.extend(self.short_order(c));
            ids
        })
    }

    /// One line, in points: each class's entries in order with their room,
    /// then those without room, below zero, then the other agents.
    pub fn line(&self) -> String {
        let mut s = String::new();
        for ((word, class), ids) in [("fable", Class::Restricted), ("plain", Class::Plain)].into_iter().zip(self.order_ids()) {
            let entries: Vec<String> = ids.iter().filter_map(|id| self.rooms.iter().find(|r| r.id == *id)).map(|r| format!("P{}={:.1}", r.id, 100.0 * r.room(class))).collect();
            s.push_str(&format!("{word}={} ", if entries.is_empty() { "-".into() } else { entries.join(",") }));
        }
        let others: Vec<String> = self.others.iter().map(|(a, l)| format!("{a}={:.1}", 100.0 * l)).collect();
        s.push_str(&format!("others={}", if others.is_empty() { "-".into() } else { others.join(",") }));
        s
    }
}
