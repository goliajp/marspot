//! What a pane is told when no account takes it.
//!
//! It stays where it is and its CLI waits for the window to come back.
//! It is told once per such stretch when that will be.  A pane that
//! needs the restricted model is not switched to another model when
//! only that model is spent everywhere: what it runs is the user's
//! choice, so it is told when the model returns instead.

use std::collections::HashMap;

use super::{Class, Quant};

#[derive(Clone, Debug, PartialEq)]
pub enum Stranded {
    /// Every account is out for any model; back in this many hours.
    AllOut { back_in_h: f64 },
    /// Only the restricted model is out everywhere; back in this many
    /// hours.  The pane keeps its model.
    RestrictedOut { back_in_h: f64 },
}

/// Why a pane of `class` has nowhere to go, when it has not.
pub fn stranded(accounts: &[Quant], class: Class) -> Option<Stranded> {
    if accounts.iter().any(|q| q.accepts(class)) {
        return None;
    }
    let soonest = |key: fn(&Quant) -> f64| accounts.iter().map(key).fold(f64::INFINITY, f64::min);
    let week_left = accounts.iter().any(|q| q.accepts(Class::Plain));
    Some(if class == Class::Restricted && week_left {
        Stranded::RestrictedOut { back_in_h: soonest(|q| q.tf) }
    } else {
        Stranded::AllOut { back_in_h: soonest(|q| q.t) }
    })
}

/// Which panes have been told, so each hears it once per stretch.
#[derive(Debug, Default)]
pub struct Told {
    told: HashMap<u64, bool>,
}

impl Told {
    /// `Some` the first time pane `sid` is found stranded since it last
    /// was not.
    pub fn tell(&mut self, sid: u64, now: Option<Stranded>) -> Option<Stranded> {
        let was = self.told.insert(sid, now.is_some()).unwrap_or(false);
        now.filter(|_| !was)
    }

    pub fn retain(&mut self, live: impl Fn(u64) -> bool) {
        self.told.retain(|sid, _| live(*sid));
    }
}
