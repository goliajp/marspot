//! How long a helper has been at work: the time it was idle is left out.

#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::plugins::helpers) struct Clock {
    /// Before `since_ms`.
    pub worked_ms: u64,
    /// At work since then (ms after the epoch); None: it is not.
    pub since_ms: Option<u64>,
    /// When it last stopped.
    pub ended_ms: u64,
}

impl Clock {
    pub fn started(at_ms: u64) -> Self {
        Clock { worked_ms: 0, since_ms: Some(at_ms), ended_ms: 0 }
    }

    pub fn runs(&self) -> bool {
        self.since_ms.is_some()
    }

    /// It stopped at `at_ms`.  A change that comes after a later one was
    /// taken in changes nothing: the facts are written a moment after
    /// they were seen.
    pub fn end(&mut self, at_ms: u64) {
        if let Some(since) = self.since_ms.filter(|s| *s <= at_ms) {
            self.worked_ms += at_ms - since;
            self.since_ms = None;
            self.ended_ms = at_ms;
        }
    }

    /// It went back to work at `at_ms`.
    pub fn begin(&mut self, at_ms: u64) {
        if self.since_ms.is_none() && self.ended_ms <= at_ms {
            self.since_ms = Some(at_ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_clock_leaves_out_idle_time_and_takes_facts_twice() {
        let mut c = Clock::started(1_000);
        c.end(4_000);
        c.begin(10_000);
        assert_eq!(c, Clock { worked_ms: 3_000, since_ms: Some(10_000), ended_ms: 4_000 });
        // written down late
        c.end(4_000);
        assert!(c.runs());
        c.end(12_000);
        c.begin(10_000);
        assert_eq!(c, Clock { worked_ms: 5_000, since_ms: None, ended_ms: 12_000 });
    }
}
