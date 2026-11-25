//! The monthly allowance some accounts have (kimi), kept beside the
//! five hours and the week.  The row's status does not count it: what
//! the panel and today's choices read stays as it was.

use marspot_peer::json::Value;

use super::feed::{Account, Window};

impl Account {
    /// A read that carried a month replaces the one held.
    pub(super) fn absorb_month(&mut self, month: Option<Window>) {
        if month.is_some() {
            self.month = month;
        }
    }

    pub(super) fn expire_month(&mut self, now: i64) {
        if self.month.as_ref().is_some_and(|w| w.expired(now)) {
            self.month = None;
        }
    }

    /// Written only where a month was read, so a feed without one is
    /// unchanged.
    pub(super) fn month_into(&self, row: &mut Value) {
        let Some(w) = &self.month else { return };
        row.set("utilization_month", Value::Num(w.util));
        row.set("reset_month", w.reset.map_or(Value::Null, Value::from));
    }
}
