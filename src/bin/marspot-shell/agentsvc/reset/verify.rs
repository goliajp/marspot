//! Checking a claim's confirmation with devops, which issued it: one
//! GET of `/api/ai/accounts/{provider}/{profile}/reset/verify`, which
//! answers 200 once for a token it issued in the last minute, with the
//! grant the user confirmed there.  Nothing else counts as confirmed.
//!
//! The address and key are the devops CLI's own files, read when a
//! claim asks and dropped after its one request.  The key goes out only
//! as the `authorization: Bearer` header of that GET, the form the
//! devops tools send, through `/usr/bin/curl`'s config on its stdin
//! (never an argument or the environment); it is never logged, written
//! or put in an answer.

use std::path::Path;

use marspot_peer::json::{self, Value};

use super::{Claim, Grant, Outcome, Res};
use crate::plugins::usage::http;

/// Where devops is and the key to ask it with.  Not `Debug`: nothing
/// may print the key.
pub(super) struct Access {
    pub base: String,
    pub key: String,
}

/// Under the home directory: the only two files of that directory read.
const URL_FILE: &str = ".config/devops/api-url";
const KEY_FILE: &str = ".config/devops/api-key";

pub(super) const NO_DEVOPS: &str =
    "devops confirms claims sent to the control socket and is not set up on this machine: claim it on the accounts panel";

/// The devops address and key, from the user's home.
pub(super) fn access() -> Result<Access, Outcome> {
    access_in(Path::new(&std::env::var_os("HOME").unwrap_or_default()))
}

pub(super) const INCOMPLETE: &str =
    "devops credential incomplete: ~/.config/devops needs both api-url and api-key to confirm a claim";

/// The devops address and key under `home`; both files must say
/// something, or the claim is refused before anything is tried.  A
/// machine without either is one without devops, which claims on the
/// accounts panel; one with only one of them has a broken devops setup.
pub(super) fn access_in(home: &Path) -> Result<Access, Outcome> {
    let read = |f: &str| std::fs::read_to_string(home.join(f)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    match (read(URL_FILE), read(KEY_FILE)) {
        (Some(base), Some(key)) => Ok(Access { base, key }),
        (None, None) => Err(Outcome::of(Res::Unavailable, NO_DEVOPS)),
        _ => Err(Outcome::of(Res::Error, INCOMPLETE)),
    }
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
