//! Moving a pane off a kimi account the service has refused.
//!
//! The one signal acted on is kimi's own record of the turn: it ended
//! `failed` with the error kimi raises for a 429 whose body says the
//! account's quota is used up.  That error shares its code with every
//! other provider error and is told apart by its name.  Nothing on the
//! screen is read, and a rate limit, which kimi retries by itself, is
//! a different error.
//!
//! What is done about it, and about an account the usage feed shows
//! out or nearly out, is in `balance`.

use super::session::Failure;

/// `turn.ended.error.name` for an account whose quota is used up.
const QUOTA_EXHAUSTED: &str = "APIProviderQuotaExhaustedError";

pub(super) fn is_quota(failure: &Failure) -> bool {
    failure.name.as_deref() == Some(QUOTA_EXHAUSTED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_quota_error_counts() {
        let f = |name: Option<&str>| Failure { code: "provider.api_error".into(), name: name.map(str::to_string), at: 1, turn: 1 };
        assert!(is_quota(&f(Some("APIProviderQuotaExhaustedError"))));
        assert!(!is_quota(&f(Some("APIProviderRateLimitError"))), "a rate limit is retried by kimi itself");
        assert!(!is_quota(&f(Some("APIStatusError"))));
        assert!(!is_quota(&f(None)));
    }
}
