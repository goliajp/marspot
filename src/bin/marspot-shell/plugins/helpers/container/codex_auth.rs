//! codex's login for a container: the account's access token, without
//! its refresh token, in an `auth.json` of the helper's own home.
//!
//! Measured with codex 0.172.0: it takes a login whose refresh token is
//! empty (a missing key is no login), and does not renew it on its own
//! until the access token has expired.  Then every renewal is refused
//! and every request with it; `auth.json` is left as it was.  The
//! account's own login on the host is never touched, so codex there
//! goes on renewing it.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use marspot_peer::json::{self, Value, obj};

/// Larger than any login codex writes.
const MAX_FILE: u64 = 64 * 1024;

/// Not `Debug`: it holds the token, and nothing may print one.
pub(super) struct Auth {
    access: String,
    id: String,
    account: Option<String>,
    mode: Option<String>,
    last_refresh: Option<String>,
    /// Unix milliseconds, from the access token.
    pub expires_at: Option<i64>,
}

fn token_word(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_graphic())
}

/// When a JWT ends, in Unix milliseconds.
pub(super) fn jwt_exp_ms(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let mut b64: String = payload.chars().map(|c| match c {
        '-' => '+',
        '_' => '/',
        c => c,
    }).collect();
    while !b64.len().is_multiple_of(4) {
        b64.push('=');
    }
    let claims = marspot_term::base64::decode(b64.as_bytes(), MAX_FILE as usize)?;
    let exp = json::parse(std::str::from_utf8(&claims).ok()?).ok()?.get("exp")?.i64()?;
    exp.checked_mul(1000)
}

pub(super) fn parse(text: &str) -> Option<Auth> {
    let v = json::parse(text).ok()?;
    let t = v.get("tokens")?;
    let word = |k: &str| t.str_at(k).filter(|s| token_word(s)).map(str::to_string);
    let access = word("access_token")?;
    let plain = |s: &&str| s.len() < 128 && token_word(s);
    Some(Auth {
        expires_at: jwt_exp_ms(&access),
        id: word("id_token")?,
        account: word("account_id"),
        mode: v.str_at("auth_mode").filter(plain).map(str::to_string),
        last_refresh: v.str_at("last_refresh").filter(plain).map(str::to_string),
        access,
    })
}

/// The login of the codex account at `home`.
pub(super) fn read(home: &Path) -> Option<Auth> {
    let f = std::fs::File::open(home.join("auth.json")).ok()?;
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    f.take(MAX_FILE).read_to_string(&mut text).ok()?;
    parse(&text)
}

/// The file the container's codex reads: the login with an empty
/// refresh token.
pub(super) fn text(a: &Auth) -> String {
    let opt = |s: &Option<String>| s.as_deref().map_or(Value::Null, Value::from);
    let tokens = obj([("id_token", a.id.as_str().into()), ("access_token", a.access.as_str().into()), ("refresh_token", "".into()), ("account_id", opt(&a.account))]);
    obj([("auth_mode", opt(&a.mode)), ("OPENAI_API_KEY", Value::Null), ("tokens", tokens), ("last_refresh", opt(&a.last_refresh))]).to_json()
}

/// Write `a` as `auth.json` in `home`, a directory a container may have
/// written to: a new file renamed into place, so that no link there is
/// followed.
pub(super) fn write(home: &Path, a: &Auth) -> std::io::Result<()> {
    let new = home.join(".auth.json.marspot");
    if std::fs::symlink_metadata(&new).is_ok() {
        std::fs::remove_file(&new)?;
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&new)?;
    let done = f.write_all(text(a).as_bytes()).and_then(|()| std::fs::rename(&new, home.join("auth.json")));
    if done.is_err() {
        // a part of a token is still a part of one
        let _ = std::fs::remove_file(&new);
    }
    done
}
