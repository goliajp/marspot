//! What a step's recorded use does on the replay's accounts: served
//! where the pane is, refused where it cannot be, held back where strict
//! protection makes an unattended other-model pane wait.
//!
//! Other-model use beyond level 1's spare takes the reserve Fable was
//! to use, except a Fable pane's own other-model use while no other
//! account holds both its classes for the stay: the guard then keeps it
//! as a Fable pane and its other-model use is accepted (`unavoidable`).

use super::super::Limits;
use super::{Mode, W};
use crate::quota::{Class, EPS};

/// A pane with the user at a boundary this recently is attended, seconds.
const ATTENDED_S: i64 = 3600;
/// Another account with this much other-model room left counts as room.
const ROOM: f64 = 0.01;

impl W<'_> {
    /// What account `id` serves of `amount` of `class`.
    pub(super) fn serve(&mut self, id: u8, class: Class, amount: f64) -> f64 {
        let Some(q) = self.accs.iter().find(|a| a.q.id == id).map(|a| a.q.clone()) else { return 0.0 };
        let served = match class {
            Class::Restricted => amount.min(q.f.max(0.0)).min(q.r.max(0.0)),
            Class::Plain => amount.min(q.r.max(0.0)),
        };
        if class == Class::Plain {
            let took = (served - (q.r - q.f.max(0.0)).max(0.0)).max(0.0);
            let room = self.accs.iter().any(|b| b.q.id != id && b.q.open && b.q.n() >= ROOM);
            let a = self.accs.iter_mut().find(|a| a.q.id == id).expect("the account");
            if room {
                self.out.phi_taken_room += took;
            }
            self.out.phi_taken += took;
            a.taken += took;
            let over = (served - a.spare.max(0.0)).max(0.0);
            a.spare -= served;
            if self.stuck {
                self.out.unavoidable += over;
            } else {
                self.out.reserve_taken += over;
                a.taken_reserve += over;
            }
        }
        let q = &mut self.accs.iter_mut().find(|a| a.q.id == id).expect("the account").q;
        q.r -= served;
        if class == Class::Restricted {
            q.f -= served;
        }
        served
    }

    /// Another open account holds both of `sid`'s classes for the stay.
    fn holds_both(&self, now: i64, sid: u64, on: u8) -> bool {
        let sp = self.speeds(sid, now);
        let stay = match self.mode {
            Mode::Planner(l) => l.dwell_h,
            Mode::Actual => Limits::STEADY.dwell_h,
        };
        self.accs.iter().any(|a| a.q.id != on && a.q.open && a.q.n() >= sp[1] * stay - EPS && a.q.phi() >= sp[0] * stay - EPS)
    }

    pub(super) fn use_(&mut self, now: i64, sid: u64, class: Class, amount: f64, ctx: u64) {
        let h = ((now - self.fx.t0) / 3600) as usize;
        if self.hours_r.len() <= h {
            self.hours_r.resize(h + 1, 0.0);
        }
        if class == Class::Restricted {
            self.hours_r[h] += amount;
        }
        let Some(p) = self.panes.get_mut(&sid) else {
            self.out.unmet += amount;
            return;
        };
        p.used.add(now, class, amount);
        if ctx > 0 {
            p.ctx = ctx;
        }
        let unattended = p.user_at.is_none_or(|t| now - t >= ATTENDED_S);
        if self.hold && class == Class::Plain && unattended && self.waits.contains(&sid) {
            self.out.waited += amount;
            return;
        }
        let (on, own, stalled) = (p.on, p.class, p.stalled);
        self.stuck = own == Class::Restricted && class == Class::Plain && !self.holds_both(now, sid, on);
        let mut rest = amount - self.serve(on, class, amount);
        if rest > EPS && class == own && !stalled {
            self.out.refusals.push((now, on, sid));
            self.refused = true;
            self.panes.get_mut(&sid).expect("the pane").stalled = true;
            if let Some(to) = matches!(self.mode, Mode::Planner(_)).then(|| self.place_refused(now, sid)).flatten() {
                self.switch(now, sid, to, None);
                rest -= self.serve(to, class, rest);
            }
        } else if rest <= EPS && class == own {
            self.panes.get_mut(&sid).expect("the pane").stalled = false;
        }
        self.out.unmet += rest.max(0.0);
    }
}
