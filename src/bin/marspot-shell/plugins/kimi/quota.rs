//! Moving a pane off a kimi account the service has refused.
//!
//! The one signal acted on is kimi's own record of the turn: it ended
//! `failed` with the error kimi raises for a 429 whose body says the
//! account's quota is used up.  That error shares its code with every
//! other provider error and is told apart by its name.  Nothing on the
//! screen is read, and a rate limit, which kimi retries by itself, is
//! a different error.
//!
//! A refused account stays shut for the rest of this run, up to the
//! length of the shortest window the service meters, so two exhausted
//! accounts do not hand a pane back and forth.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::session::Failure;

/// `turn.ended.error.name` for an account whose quota is used up.
const QUOTA_EXHAUSTED: &str = "APIProviderQuotaExhaustedError";

/// The shortest of the service's usage windows.
const SHUT_FOR: Duration = Duration::from_secs(5 * 3600);

pub(super) fn is_quota(failure: &Failure) -> bool {
    failure.name.as_deref() == Some(QUOTA_EXHAUSTED)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// This failure was already dealt with.
    Seen,
    /// Every other account is shut, or there is none.
    Stay,
    /// Move the pane to this profile.
    Move(u8),
}

#[derive(Default)]
pub(super) struct Refusals {
    shut: HashMap<PathBuf, Instant>,
    /// The failed turn last acted on per pane: its session and number.
    acted: HashMap<u64, (String, i64)>,
}

fn real(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

impl Refusals {
    /// What to do about the quota failure pane `sid` shows, running
    /// under `home`.  Each failed turn is answered once.
    pub(super) fn verdict(&mut self, sid: u64, home: &Path, session: &str, turn: i64, accounts: &[(u8, PathBuf)], now: Instant) -> Verdict {
        if self.acted.get(&sid).is_some_and(|(s, t)| s == session && *t == turn) {
            return Verdict::Seen;
        }
        self.acted.insert(sid, (session.to_string(), turn));
        self.shut.retain(|_, at| now.duration_since(*at) < SHUT_FOR);
        self.shut.insert(real(home), now);
        match accounts.iter().find(|(_, dir)| !self.shut.contains_key(&real(dir))) {
            Some((n, _)) => Verdict::Move(*n),
            None => Verdict::Stay,
        }
    }

    pub(super) fn retain(&mut self, live: impl Fn(u64) -> bool) {
        self.acted.retain(|sid, _| live(*sid));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accounts() -> Vec<(u8, PathBuf)> {
        vec![(1, PathBuf::from("/h/.kimi-code-profile-1")), (2, PathBuf::from("/h/.kimi-code-profile-2"))]
    }

    #[test]
    fn only_the_quota_error_counts() {
        let f = |name: Option<&str>| Failure { code: "provider.api_error".into(), name: name.map(str::to_string), turn: 1 };
        assert!(is_quota(&f(Some("APIProviderQuotaExhaustedError"))));
        assert!(!is_quota(&f(Some("APIProviderRateLimitError"))), "a rate limit is retried by kimi itself");
        assert!(!is_quota(&f(Some("APIStatusError"))));
        assert!(!is_quota(&f(None)));
    }

    #[test]
    fn a_refused_account_is_left_for_another_and_stays_shut() {
        let mut r = Refusals::default();
        let now = Instant::now();
        let one = Path::new("/h/.kimi-code-profile-1");
        let two = Path::new("/h/.kimi-code-profile-2");
        assert_eq!(r.verdict(7, one, "session_a", 3, &accounts(), now), Verdict::Move(2));
        assert_eq!(r.verdict(7, one, "session_a", 3, &accounts(), now), Verdict::Seen, "one failed turn, one move");
        // the account it moved to is refused as well: nowhere left
        assert_eq!(r.verdict(7, two, "session_b", 0, &accounts(), now), Verdict::Stay);
        // another pane on the first account is not sent to a shut one
        assert_eq!(r.verdict(8, one, "session_c", 5, &accounts(), now), Verdict::Stay);
        let later = now + SHUT_FOR;
        assert_eq!(r.verdict(8, one, "session_c", 6, &accounts(), later), Verdict::Move(2), "the window has passed");
    }

    #[test]
    fn with_one_account_nothing_moves() {
        let mut r = Refusals::default();
        let only = vec![(0, PathBuf::from("/h/.kimi-code"))];
        let now = Instant::now();
        assert_eq!(r.verdict(1, Path::new("/h/.kimi-code"), "session_a", 0, &only, now), Verdict::Stay);
        assert_eq!(r.verdict(1, Path::new("/h/.kimi-code"), "session_a", 0, &only, now), Verdict::Seen);
        assert_eq!(r.verdict(1, Path::new("/h/.kimi-code"), "session_a", 0, &[], now), Verdict::Seen);
        r.retain(|_| false);
        assert!(r.acted.is_empty());
    }
}
