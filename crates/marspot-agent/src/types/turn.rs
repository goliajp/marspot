//! One reply's token counts, as an adapter reads them from a session's
//! record for the switch-cost measure.

/// One reply's counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Turn {
    pub at: i64,
    pub input: u64,
    pub write_5m: u64,
    pub write_1h: u64,
    pub read: u64,
    pub output: u64,
}

impl Turn {
    /// Everything the reply sent.
    pub fn prompt(&self) -> u64 {
        self.input + self.write_5m + self.write_1h + self.read
    }

    /// What it sent that the cache did not hold.
    pub fn uncached(&self) -> u64 {
        self.input + self.write_5m + self.write_1h
    }

    /// Less than half of what it sent came from the cache.
    pub fn cold(&self) -> bool {
        self.read * 2 < self.prompt()
    }
}
