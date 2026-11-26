//! The `reset.*` requests: claim one stored rate-limit reset of an
//! agent account, after the caller's confirmation has been checked
//! with the place that issued it, and list what is known of every
//! account's resets.

use super::{Args, bad};
use crate::json::Value;

/// The longest word, reference or id a claim carries.
const MAX_WORD: usize = 64;
/// The longest confirmation token.
const MAX_TOKEN: usize = 512;
/// The longest reason.
const MAX_REASON: usize = 500;

#[derive(Clone, Debug, PartialEq)]
pub struct ResetClaimArgs {
    /// `claude` or `codex`.
    pub provider: String,
    pub profile: u8,
    /// `full`, `weekly`, `session`, `fable` or `codex_full`, or a word a
    /// later provider adds.
    pub kind: String,
    /// The grant as the readings name it (`codex:1:g2`).
    pub grant_ref: String,
    /// Asking again with the same id gives the same answer, never a
    /// second claim.
    pub request_id: String,
    /// The single-use token the confirming side issued.
    pub confirm: String,
    pub reason: String,
}

fn word(a: &Args, k: &str, max: usize, ok: impl Fn(char) -> bool) -> Result<String, super::Reject> {
    let v = a.str(k)?.ok_or_else(|| bad(format!("`{k}` is required")))?;
    if v.is_empty() || v.len() > max || !v.chars().all(ok) {
        return Err(bad(format!("`{k}` is at most {max} characters of the allowed set")));
    }
    Ok(v)
}

pub(super) fn claim(a: &Args) -> Result<ResetClaimArgs, super::Reject> {
    let provider = word(a, "provider", MAX_WORD, |c| c.is_ascii_lowercase())?;
    if !matches!(provider.as_str(), "claude" | "codex") {
        return Err(bad("`provider` is claude or codex"));
    }
    let profile = match a.uint("profile")? {
        Some(n @ 0..=255) => n as u8,
        _ => return Err(bad("`profile` is required: an account number from 0 to 255")),
    };
    let reason = a.str("reason")?.unwrap_or_default();
    if reason.chars().count() > MAX_REASON || reason.chars().any(char::is_control) {
        return Err(bad(format!("`reason` is one line of at most {MAX_REASON} characters")));
    }
    Ok(ResetClaimArgs {
        provider,
        profile,
        kind: word(a, "kind", MAX_WORD, |c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')?,
        grant_ref: word(a, "grant_ref", MAX_WORD, |c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-'))?,
        request_id: word(a, "request_id", MAX_WORD, |c| c.is_ascii_hexdigit() || c == '-')?,
        confirm: word(a, "confirm", MAX_TOKEN, |c| c.is_ascii_graphic() && c != '"' && c != '\\')?,
        reason,
    })
}

pub(super) fn claim_json(c: &ResetClaimArgs) -> Vec<(String, Value)> {
    vec![
        ("provider".into(), c.provider.as_str().into()),
        ("profile".into(), i64::from(c.profile).into()),
        ("kind".into(), c.kind.as_str().into()),
        ("grant_ref".into(), c.grant_ref.as_str().into()),
        ("request_id".into(), c.request_id.as_str().into()),
        ("confirm".into(), c.confirm.as_str().into()),
        ("reason".into(), c.reason.as_str().into()),
    ]
}

#[cfg(test)]
mod pane_control_reset_tests {
    use super::super::{Op, parse};
    use crate::control::ErrorCode;

    const LINE: &str = r#"{"op":"reset.claim","args":{"provider":"codex","profile":2,"kind":"codex_full","grant_ref":"codex:2:g1","request_id":"0b6a7c1e-3f1d-4a8e-9c55-2d7f0e9b1a42","confirm":"tok_abc.123","reason":"week spent, 5 days to go"}}"#;

    #[test]
    fn pane_control_reset_claim_reads_and_writes_back() {
        let r = parse(LINE).unwrap();
        let Op::ResetClaim(a) = &r.op else { panic!("{r:?}") };
        assert_eq!((a.provider.as_str(), a.profile, a.kind.as_str(), a.grant_ref.as_str()), ("codex", 2, "codex_full", "codex:2:g1"));
        assert_eq!(parse(&r.to_json()).unwrap(), r);
        assert!(!r.op.is_read_only());
        assert_eq!(r.op.name(), "reset.claim");
        let s = parse(r#"{"op":"reset.status"}"#).unwrap();
        assert!(s.op.is_read_only() && s.op.name() == "reset.status");
    }

    #[test]
    fn pane_control_reset_claim_refuses_what_is_missing_or_malformed() {
        for (from, to) in [
            (r#""provider":"codex""#, r#""provider":"gemini""#),
            (r#""profile":2"#, r#""profile":256"#),
            (r#""profile":2"#, r#""profile":"2""#),
            (r#""kind":"codex_full""#, r#""kind":"Full reset""#),
            (r#""grant_ref":"codex:2:g1""#, r#""grant_ref":"a b""#),
            (r#""request_id":"0b6a7c1e-3f1d-4a8e-9c55-2d7f0e9b1a42""#, r#""request_id":"zz""#),
            (r#""confirm":"tok_abc.123""#, r#""confirm":"""#),
            (r#""reason":"week spent, 5 days to go""#, r#""reason":"two\nlines""#),
        ] {
            let line = LINE.replace(from, to);
            assert_eq!(parse(&line).unwrap_err().code, ErrorCode::BadRequest, "{line}");
        }
        let no_reason = LINE.replace(r#","reason":"week spent, 5 days to go""#, "");
        let Op::ResetClaim(a) = parse(&no_reason).unwrap().op else { panic!() };
        assert_eq!(a.reason, "");
    }
}
