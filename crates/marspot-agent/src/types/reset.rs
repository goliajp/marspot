//! Stored resets as the agents' usage feeds describe them: one kind of
//! reset of one account as supply, and whether two readings of an
//! account's resets agree.  The adapters and the usage service produce
//! them; the scheduler counts on them.

/// One kind of stored reset of one account, as the allocation counts it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Supply {
    /// Brings the week back.
    pub week: bool,
    /// Brings the restricted window back.
    pub window: bool,
    /// What the restricted window used comes back to the week as well.
    pub gives_back_week: bool,
    /// What a reset of the restricted window alone also takes off the
    /// week's usage.
    pub week_cut: WeekCut,
    /// Using it moves the week's renewal to a week after the use.
    pub moves_week: bool,
    /// Usable only once the account is at the limit of what it brings back.
    pub requires_limit: bool,
    /// Hours until it may be used; 0 when it may be used now.
    pub starts_h: f64,
    /// Hours until it ends; `None` when it does not.
    pub ends_h: Option<f64>,
    pub left: u32,
    /// Usable only while the limit it brings back has less than this
    /// share of a full one left; `None` when any amount will do.
    pub usable_below_left: Option<f64>,
}

/// How much of the week's usage a reset of the restricted window
/// takes off as well.  Amounts are in weeks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WeekCut {
    Nothing,
    /// This share of what the week has used.
    ShareOfUsed(f64),
    /// This much, at most what the week has used.
    Fixed(f64),
}

impl WeekCut {
    /// What comes back to the week when it has used `used`.
    pub fn of(self, used: f64) -> f64 {
        let used = used.clamp(0.0, 1.0);
        match self {
            WeekCut::Nothing => 0.0,
            WeekCut::ShareOfUsed(s) => s.clamp(0.0, 1.0) * used,
            WeekCut::Fixed(w) => w.clamp(0.0, used),
        }
    }
}

/// Two readings this far apart are not compared: a reset may have been
/// used or granted between them.
pub const AGREE_WITHIN_S: i64 = 15 * 60;

/// What one reading says about an account's resets, for comparison.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Count {
    pub observed_at: i64,
    pub left: u32,
    /// The earliest end of one with any left, where the reading says.
    pub next_expiry: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agreement {
    Same,
    Differs,
    /// Too far apart in time to say.
    Apart,
}

/// Whether two readings of one account agree.  An end only one of them
/// gives is not compared.
pub fn agree(a: Count, b: Count) -> Agreement {
    if (a.observed_at - b.observed_at).abs() > AGREE_WITHIN_S {
        return Agreement::Apart;
    }
    let ends_differ = matches!((a.next_expiry, b.next_expiry), (Some(x), Some(y)) if x != y);
    if a.left != b.left || ends_differ { Agreement::Differs } else { Agreement::Same }
}
