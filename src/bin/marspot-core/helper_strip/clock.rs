//! The second a window's strips show, the same for every helper in it.

use std::time::{Duration, Instant};

const SECOND: Duration = Duration::from_secs(1);
/// How far the wall clock may be off before it is read again.
const OFF_MS: u64 = 2_000;

struct Clock {
    window: u32,
    /// When the second in view began, and what time it was (ms after
    /// the epoch).
    at: Instant,
    ms: u64,
}

/// A clock per window that draws a strip.
#[derive(Default)]
pub(super) struct Clocks(Vec<Clock>);

impl Clocks {
    /// The time the strips of `window` show now.  It moves by whole
    /// seconds, so a frame drawn for another reason changes none.
    pub(super) fn ms(&mut self, window: u32, now: Instant, now_ms: u64) -> u64 {
        let i = match self.0.iter().position(|c| c.window == window) {
            Some(i) => i,
            None => {
                self.0.push(Clock { window, at: now, ms: now_ms });
                self.0.len() - 1
            }
        };
        let c = &mut self.0[i];
        let whole = now.saturating_duration_since(c.at).as_secs();
        c.at += SECOND * whole.min(u32::MAX as u64) as u32;
        c.ms += whole * 1000;
        if now_ms.abs_diff(c.ms) > OFF_MS {
            (c.at, c.ms) = (now, now_ms);
        }
        c.ms
    }

    /// When the second `window` shows is over.
    pub(super) fn due(&self, window: u32) -> Option<Instant> {
        self.0.iter().find(|c| c.window == window).map(|c| c.at + SECOND)
    }

    pub(super) fn keep(&mut self, open: impl Fn(u32) -> bool) {
        self.0.retain(|c| open(c.window));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_moves_by_whole_seconds_and_follows_the_wall() {
        let mut c = Clocks::default();
        let t = Instant::now();
        let at = |ms| t + Duration::from_millis(ms);
        assert_eq!(c.ms(1, t, 5_000_400), 5_000_400);
        assert_eq!(c.ms(1, at(999), 5_001_399), 5_000_400);
        assert_eq!(c.due(1), Some(at(1_000)));
        assert_eq!(c.ms(1, at(1_004), 5_001_404), 5_001_400, "late by a little: the same phase");
        assert_eq!(c.due(1), Some(at(2_000)));
        assert_eq!(c.ms(1, at(9_500), 5_009_900), 5_009_400, "out of sight for a while");
        assert_eq!(c.ms(1, at(9_600), 9_000_000), 9_000_000, "the clock was set");
        assert_eq!(c.due(2), None);
        assert_eq!(c.ms(2, at(9_700), 7), 7);
        c.keep(|w| w == 2);
        assert_eq!((c.due(1), c.due(2)), (None, Some(at(10_700))));
    }
}
