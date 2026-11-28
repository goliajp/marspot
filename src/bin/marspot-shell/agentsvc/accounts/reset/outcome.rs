//! What one claim came to, and the answer line made of it.

use marspot_peer::json::{Value, obj};

macro_rules! results {
    ($($var:ident = $s:literal),* $(,)?) => {
        /// The words a claim's `result` is one of.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum Res { $($var,)* }
        impl Res {
            pub(crate) fn as_str(self) -> &'static str {
                match self { $(Res::$var => $s,)* }
            }
            pub(crate) fn parse(s: &str) -> Option<Res> {
                match s { $($s => Some(Res::$var),)* _ => None }
            }
        }
    };
}

results! {
    Reset = "reset",
    AlreadyUsed = "already_used",
    NotLimited = "not_limited",
    Cooldown = "cooldown",
    Ineligible = "ineligible",
    Unavailable = "unavailable",
    NoCredit = "no_credit",
    AccountBusy = "account_busy",
    Busy = "busy",
    Error = "error",
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Outcome {
    pub result: Res,
    /// Resets of that account left afterwards, where a reading says.
    pub left_after: Option<u32>,
    /// The account's resets block read afterwards, where one was.
    pub observed_after: Option<Value>,
    /// One line: what the provider or the CLI said, or why nothing was
    /// tried.
    pub detail: String,
    /// Not known to have ended: asking again with the same request id
    /// tries again (the provider's own idempotency key makes that safe).
    pub again: bool,
    /// Ended before anything that uses the grant was sent to the
    /// provider; not written down, only read by the claim that got it.
    pub untried: bool,
}

impl Outcome {
    pub(crate) fn of(result: Res, detail: &str) -> Outcome {
        Outcome { result, left_after: None, observed_after: None, detail: detail.to_string(), again: false, untried: false }
    }

    /// An end before anything that uses the grant was sent.
    pub(crate) fn untried(result: Res, detail: &str) -> Outcome {
        Outcome { untried: true, ..Outcome::of(result, detail) }
    }

    /// An error after which the same request may be asked again.
    pub(crate) fn again(result: Res, detail: &str) -> Outcome {
        Outcome { again: true, ..Outcome::of(result, detail) }
    }

    /// The attempt ended and asking again with the same request id gets
    /// this answer; `busy` means another claim was running, so a later
    /// ask with that id is a new attempt, as is one marked `again`.
    pub(crate) fn is_final(&self) -> bool {
        self.result != Res::Busy && !self.again
    }

    /// The answer to the asker of `request_id`.
    pub(crate) fn reply(&self, request_id: &str) -> Value {
        obj([
            ("result", self.result.as_str().into()),
            ("request_id", request_id.into()),
            ("left_after", self.left_after.map_or(Value::Null, |n| i64::from(n).into())),
            ("observed_after", self.observed_after.clone().unwrap_or(Value::Null)),
            ("detail", self.detail.as_str().into()),
        ])
    }
}
