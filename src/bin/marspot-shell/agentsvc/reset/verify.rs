//! Checking a claim's confirmation with devops, which issued it: one
//! GET of `/api/ai/accounts/{provider}/{profile}/reset/verify`, which
//! answers 200 once for a token it issued in the last minute, with the
//! grant the user confirmed there.  Nothing else counts as confirmed.

use marspot_peer::json::{self, Value};

use super::{Claim, Grant, Outcome, Res};
use crate::plugins::usage::http;

/// Where devops is and the key to ask it with.  Not `Debug`: nothing
/// may print the key.
pub(super) struct Access {
    pub base: String,
    pub key: String,
}

/// The devops address and key marspot asks with.  marspot reads no
/// devops credential anywhere yet, so there is none, and a claim from
/// the socket is refused before anything is tried.
pub(super) fn access() -> Option<Access> {
    None
}

/// `s` as one query value.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub(super) fn url(base: &str, c: &Claim, confirm: &str) -> String {
    format!(
        "{}/api/ai/accounts/{}/{}/reset/verify?confirm={}&request_id={}&kind={}",
        base.trim_end_matches('/'),
        c.provider.as_str(),
        c.profile,
        encode(confirm),
        encode(&c.request_id),
        encode(&c.grant.kind)
    )
}

fn time_at(v: &Value, key: &str) -> Option<i64> {
    match v.get(key)? {
        Value::Num(n) => Some(*n as i64),
        Value::Str(s) => marspot::cc_usage::parse_iso_utc(s.clone()),
        _ => None,
    }
}

/// The grant devops confirmed, from its answer; refused unless it is a
/// 200 naming exactly this account, kind, grant and request.
pub(super) fn check(status: u16, body: &str, c: &Claim) -> Result<Grant, Outcome> {
    if status != 200 {
        return Err(Outcome::of(Res::Error, &format!("devops did not confirm this claim (HTTP {status})")));
    }
    let v = json::parse(body).map_err(|_| Outcome::of(Res::Error, "devops answered with something that is not JSON"))?;
    let same = v.str_at("provider") == Some(c.provider.as_str())
        && v.get("profile").and_then(Value::i64) == Some(i64::from(c.profile))
        && v.str_at("kind") == Some(c.grant.kind.as_str())
        && v.str_at("request_id") == Some(c.request_id.as_str())
        && v.str_at("grant_ref").is_none_or(|r| r == c.grant.grant_ref);
    if !same {
        return Err(Outcome::of(Res::Error, "devops confirmed a different account, kind, grant or request"));
    }
    Ok(Grant { starts_at: time_at(&v, "starts_at"), ends_at: time_at(&v, "ends_at"), ..c.grant.clone() })
}

/// Ask devops whether `confirm` confirms `c`.
pub(super) fn verify(access: &Access, c: &Claim, confirm: &str) -> Result<Grant, Outcome> {
    match http::get_bare(&url(&access.base, c, confirm), &access.key) {
        Some(r) => check(r.status, &r.body, c),
        None => Err(Outcome::of(Res::Error, "devops could not be reached to check the confirmation")),
    }
}
