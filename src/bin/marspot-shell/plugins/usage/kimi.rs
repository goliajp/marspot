//! Kimi accounts: one per kimi home, read from the endpoint the CLI's
//! own `/usage` panel calls.
//!
//! The login is read and never refreshed or written.  A kimi access
//! token lasts minutes, and the CLI renews it while a session is in
//! use; a renewal from here would race the CLI's own.  So an account is
//! asked only while the token its home holds has not run out, and
//! otherwise keeps its last numbers until the CLI has renewed it --
//! which it does exactly when the account is being spent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use marspot::kimi_usage::account_name;
use marspot_peer::json::{self, Value};
use marspot_term::lx_info;

use super::claude::{outcome_with, Outcome, Waits};
use super::feed::{Account, Reading, Window};
use super::http;
use crate::plugins::kimi::{default_home, managed, profile_dirs_in};

const MAINLAND_BASE: &str = "https://api.kimi.com/coding/v1";
const GLOBAL_BASE: &str = "https://api.kimi.ai/coding/v1";
/// The name a login's token is kept under when its config names none.
const DEFAULT_SLOT: &str = "kimi-code";
/// A token this close to running out is not sent: it would run out on
/// the way.
const SPARE_SECS: i64 = 15;
/// How often an account's name is asked for again.
const LABEL_EVERY_SECS: i64 = 86_400;

#[derive(Debug, PartialEq)]
pub(super) struct Home {
    pub name: String,
    pub path: PathBuf,
}

/// Every kimi home under `home`, each once: the numbered profiles,
/// then the default home unless it is a link to one of them.
pub(super) fn homes(home: &Path) -> Vec<Home> {
    let mut out: Vec<Home> = Vec::new();
    for (n, candidate) in profile_dirs_in(home).into_iter().chain([(0, default_home(home))]) {
        let Ok(real) = std::fs::canonicalize(&candidate) else { continue };
        if real.is_dir() && !out.iter().any(|h| h.path == real) {
            out.push(Home { name: account_name(n), path: real });
        }
    }
    out
}

/// Is this an address on this machine?
pub(super) fn is_local(base: &str) -> bool {
    base.starts_with("http://127.0.0.1:")
}

/// The managed API a home's kimi talks to, in the order kimi settles
/// it: `KIMI_CODE_BASE_URL`, the address its login wrote into
/// `config.toml`, the region the installer marked in `<home>/region`,
/// the mainland service.  None for an address a token is not sent to.
pub(super) fn base_url(env: Option<&str>, configured: Option<&str>, marker: Option<&str>) -> Option<String> {
    let by_region = if marker.map(str::trim) == Some("global") { GLOBAL_BASE } else { MAINLAND_BASE };
    let base = env.or(configured).unwrap_or(by_region).trim_end_matches('/');
    (base.starts_with("https://") || is_local(base)).then(|| base.to_string())
}

/// The file a home's token is in.
pub(super) fn credentials(home: &Path, oauth_key: Option<&str>) -> PathBuf {
    let slot = oauth_key
        .map(|k| k.rsplit('/').next().unwrap_or(k))
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        .unwrap_or(DEFAULT_SLOT);
    home.join("credentials").join(format!("{slot}.json"))
}

/// Not `Debug`: it holds the token, and nothing may print one.
pub(super) struct Login {
    pub token: String,
    /// Unix seconds; 0 when the file gives none, which is how kimi
    /// writes a login that has been signed out.
    pub expires_at: i64,
}

pub(super) fn parse_login(text: &str) -> Option<Login> {
    let v = json::parse(text.trim()).ok()?;
    let token = v.str_at("access_token").filter(|t| !t.is_empty())?;
    Some(Login { token: token.to_string(), expires_at: v.get("expires_at").and_then(Value::i64).unwrap_or(0) })
}

/// Where an account stands with the collector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum State {
    Collecting,
    /// The token has run out; the CLI renews it when a session there
    /// is next used.
    TokenExpired,
    /// The service turned down a token that had not run out.
    Refused,
    NoLogin,
}

impl State {
    fn says(self) -> &'static str {
        match self {
            State::Collecting => "collecting",
            State::TokenExpired => "token expired, waiting for the CLI to refresh it",
            State::Refused => "the service refused the token; waiting for the CLI to write a new one",
            State::NoLogin => "no login",
        }
    }
}

/// `refused`: the service turned this very file down.
pub(super) fn state_of(login: Option<&Login>, refused: bool, now: i64) -> State {
    match login {
        None => State::NoLogin,
        Some(l) if l.expires_at <= now + SPARE_SECS => State::TokenExpired,
        Some(_) if refused => State::Refused,
        Some(_) => State::Collecting,
    }
}

/// An RFC 3339 time as unix seconds, its offset applied.
fn unix_of(stamp: &str) -> Option<i64> {
    let utc = marspot::cc_usage::parse_iso_utc(stamp.to_string())?;
    let zone = stamp.get(19..)?.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let hm = |s: &str| Some(s.get(..2)?.parse::<i64>().ok()? * 3_600 + s.get(3..5)?.parse::<i64>().ok()? * 60);
    match zone.as_bytes().first() {
        Some(b'+') => Some(utc - hm(&zone[1..])?),
        Some(b'-') => Some(utc + hm(&zone[1..])?),
        _ => Some(utc),
    }
}

/// The two windows in a `/usages` body.  `used_ratio` is a fraction,
/// written as a number or as a string; `reset_time` is an RFC 3339
/// time and may be missing.  A body with neither window says nothing:
/// a row of blanks would read as an account with everything left.
pub(super) fn parse_usage(body: &str, now: i64) -> Option<Reading> {
    let v = json::parse(body).ok()?;
    let usages = v.get("usages")?;
    let window = |key: &str| {
        let entry = usages.get(key)?;
        let ratio = match entry.get("used_ratio")? {
            Value::Num(n) => *n,
            Value::Str(s) => s.parse().ok()?,
            _ => return None,
        };
        if !ratio.is_finite() {
            return None;
        }
        let reset = entry.str_at("reset_time").and_then(unix_of);
        Some(Window::new(ratio.clamp(0.0, 1.0), reset, now))
    };
    let r = Reading { five_hour: window("limit_5h"), seven_day: window("limit_7d"), ..Reading::default() };
    (r.five_hour.is_some() || r.seven_day.is_some()).then_some(r)
}

/// What a `/me` body calls the account: its address, or else its
/// nickname.
pub(super) fn parse_label(body: &str) -> Option<String> {
    let v = json::parse(body).ok()?;
    v.str_at("user_id").filter(|id| !id.is_empty())?;
    let field = |key: &str| v.str_at(key).filter(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control));
    field("email").or(field("nickname")).map(str::to_string)
}

/// What the collector remembers about kimi between passes.
#[derive(Default)]
pub(super) struct Kimi {
    states: HashMap<String, State>,
    /// Per account, the token file the service turned down, by when it
    /// was written: it is not sent again.
    refused: HashMap<String, SystemTime>,
    /// Per account, when its name was last read.
    labelled: HashMap<String, i64>,
}

impl Kimi {
    /// One pass over every kimi home under `home`.  With `local_only`,
    /// only an account whose API is on this machine is asked.
    pub(super) fn pass(&mut self, home: &Path, waits_file: &Path, held: Vec<Account>, now: i64, local_only: bool) -> Vec<Account> {
        let env_base = std::env::var("KIMI_CODE_BASE_URL").ok();
        let mut waits = Waits::parse(&std::fs::read_to_string(waits_file).unwrap_or_default());
        let before = waits.to_json();
        waits.prune(now);
        let mut held = held;
        let mut out = Vec::new();
        for h in homes(home) {
            let mut account = match held.iter().position(|a| a.name == h.name) {
                Some(i) => held.swap_remove(i),
                None => Account { name: h.name.clone(), ..Account::default() },
            };
            let config = managed(&h.path);
            let marker = std::fs::read_to_string(h.path.join("region")).ok();
            let base = base_url(env_base.as_deref(), config.base_url.as_deref(), marker.as_deref());
            if let Some(base) = base.filter(|b| !local_only || is_local(b)) {
                let file = credentials(&h.path, config.oauth_key.as_deref());
                let state = self.poll(&base, &file, &mut account, &mut waits, now);
                if self.states.insert(h.name.clone(), state) != Some(state) {
                    lx_info!("usage.kimi.state", state.says(), account = h.name.clone());
                }
            }
            account.expire(now);
            out.push(account);
        }
        let known = |name: &String| out.iter().any(|a| a.name == *name);
        self.states.retain(|name, _| known(name));
        self.refused.retain(|name, _| known(name));
        self.labelled.retain(|name, _| known(name));
        if waits.to_json() != before {
            let _ = super::feed::write_atomic(waits_file, &waits.to_json());
        }
        out
    }

    /// Ask the service about one account, when its token allows.
    fn poll(&mut self, base: &str, file: &Path, account: &mut Account, waits: &mut Waits, now: i64) -> State {
        let login = std::fs::read_to_string(file).ok().and_then(|t| parse_login(&t));
        let written = std::fs::metadata(file).and_then(|m| m.modified()).ok();
        let refused = written.is_some() && self.refused.get(&account.name) == written.as_ref();
        let state = state_of(login.as_ref(), refused, now);
        let Some(login) = login.filter(|_| state == State::Collecting && !waits.blocked(&account.name, now)) else {
            return state;
        };
        match outcome_with(http::get_bare(&format!("{base}/usages"), &login.token).as_ref(), now, parse_usage) {
            Outcome::Read(reading) => {
                account.absorb(reading, now);
                self.label(base, &login, account, now);
            }
            Outcome::Wait(until) => waits.wait(&account.name, until),
            // the row keeps its numbers: a login that is really gone is
            // for the session there to find out, not for a feed to
            // empty the account on
            Outcome::AuthError => {
                if let Some(at) = written {
                    self.refused.insert(account.name.clone(), at);
                }
                return State::Refused;
            }
            Outcome::Nothing => {}
        }
        State::Collecting
    }

    /// Read the account's name once a day; it does not change with the
    /// token.
    fn label(&mut self, base: &str, login: &Login, account: &mut Account, now: i64) {
        if self.labelled.get(&account.name).is_some_and(|at| now - at < LABEL_EVERY_SECS) {
            return;
        }
        self.labelled.insert(account.name.clone(), now);
        let answer = http::get_bare(&format!("{base}/me"), &login.token).filter(|r| r.status == 200);
        if let Some(label) = answer.and_then(|r| parse_label(&r.body)) {
            account.email = label;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_290_114;

    /// The shape kimi 2.5.3 reads out of `/usages`, with made-up
    /// figures: two more windows and a wallet this does not use.
    const BODY: &str = r#"{
      "usages": {
        "limit_5h": {"used_ratio": 0.37, "reset_time": "2026-11-10T08:00:00Z"},
        "limit_7d": {"used_ratio": "0.82", "reset_time": "2026-11-14T00:00:00.250+08:00"},
        "limit_month_total": {"used_ratio": 0.5},
        "limit_month_code": {"used_ratio": 0.1, "reset_time": ""}
      },
      "boosterWallet": {"balance": {"type": "BOOSTER", "amount": 5000000, "amountLeft": "2500000"}}
    }"#;

    #[test]
    fn a_usage_body_becomes_the_five_hour_and_seven_day_windows() {
        let r = parse_usage(BODY, NOW).unwrap();
        assert_eq!(r.five_hour, Some(Window::new(0.37, Some(1_794_297_600), NOW)));
        // midnight at +08:00 is 16:00 the day before
        assert_eq!(r.seven_day, Some(Window::new(0.82, Some(1_794_585_600), NOW)));
        assert!(r.models.is_empty() && r.credits.is_none() && !r.limit_reached);
    }

    #[test]
    fn a_window_may_be_missing_over_full_or_already_over() {
        let only_week = r#"{"usages":{"limit_7d":{"used_ratio":1.25}}}"#;
        let r = parse_usage(only_week, NOW).unwrap();
        assert_eq!((r.five_hour, r.seven_day), (None, Some(Window::new(1.0, None, NOW))), "a ratio past full is full");

        let past = r#"{"usages":{"limit_5h":{"used_ratio":0.9,"reset_time":"2026-11-10T00:00:00Z"},"limit_7d":{"used_ratio":-0.1}}}"#;
        let r = parse_usage(past, NOW).unwrap();
        assert_eq!(r.five_hour.as_ref().and_then(|w| w.reset), Some(1_794_268_800), "kept as the service gave it");
        assert_eq!(r.seven_day.map(|w| w.util), Some(0.0));
        let mut account = Account::default();
        account.absorb(parse_usage(past, NOW).unwrap(), NOW);
        account.expire(NOW + 16 * 60);
        assert!(account.five_hour.is_none() && account.seven_day.is_some(), "a figure about a period that ended is dropped");

        for nothing in [
            r#"{"usages":{}}"#,
            r#"{"usages":{"limit_5h":{"reset_time":"2026-11-10T08:00:00Z"}}}"#,
            r#"{"usages":{"limit_5h":{"used_ratio":"lots"},"limit_7d":{"used_ratio":null}}}"#,
            r#"{"usages":{"limit_month_total":{"used_ratio":0.5}}}"#,
            r#"{"error":{"message":"no"}}"#,
            "[]",
            "not json",
        ] {
            assert_eq!(parse_usage(nothing, NOW), None, "{nothing}");
        }
    }

    #[test]
    fn an_account_is_named_by_its_address_or_else_its_nickname() {
        let me = r#"{"user_id":"u_123","nickname":"moonwalker","status":"USER_STATUS_NORMAL","user_level":30,"email":"ada@example.jp","phone":{"country_code":"86","number":"176****0000"}}"#;
        assert_eq!(parse_label(me).as_deref(), Some("ada@example.jp"));
        assert_eq!(parse_label(&me.replace(r#""email":"ada@example.jp","#, "")).as_deref(), Some("moonwalker"));
        assert_eq!(parse_label(r#"{"user_id":"u_123","nickname":""}"#), None);
        assert_eq!(parse_label(r#"{"nickname":"moonwalker"}"#), None, "kimi reads a body without a user id as malformed");
        assert_eq!(parse_label(r#"{"user_id":"u_123","nickname":"a\nb"}"#), None);
    }

    #[test]
    fn a_token_is_sent_only_while_it_has_not_run_out() {
        let login = |expires_at: i64| parse_login(&format!(r#"{{"access_token":"tok-1","refresh_token":"r","expires_at":{expires_at},"scope":"","token_type":"Bearer","expires_in":900}}"#));
        assert_eq!(state_of(login(NOW + 600).as_ref(), false, NOW), State::Collecting);
        assert_eq!(state_of(login(NOW + SPARE_SECS).as_ref(), false, NOW), State::TokenExpired, "about to run out");
        assert_eq!(state_of(login(NOW - 3_000).as_ref(), false, NOW), State::TokenExpired);
        assert_eq!(state_of(login(NOW - 3_000).as_ref(), true, NOW), State::TokenExpired);
        assert_eq!(state_of(login(NOW + 600).as_ref(), true, NOW), State::Refused, "this very file was turned down");
        assert_eq!(state_of(None, false, NOW), State::NoLogin);
        // a login kimi has signed out, and one with no expiry at all
        assert!(parse_login(r#"{"access_token":"","refresh_token":"","expires_at":0}"#).is_none());
        let undated = parse_login(r#"{"access_token":"tok-1"}"#).unwrap();
        assert_eq!(state_of(Some(&undated), false, NOW), State::TokenExpired);
        assert!(parse_login("not json").is_none());
    }

    #[test]
    fn the_api_is_the_one_kimi_itself_would_use() {
        assert_eq!(base_url(None, None, None).as_deref(), Some(MAINLAND_BASE));
        assert_eq!(base_url(None, None, Some("global\n")).as_deref(), Some(GLOBAL_BASE));
        assert_eq!(base_url(None, None, Some("mainland-cn\n")).as_deref(), Some(MAINLAND_BASE));
        assert_eq!(base_url(None, Some("https://api.kimi.ai/coding/v1/"), Some("mainland-cn")).as_deref(), Some(GLOBAL_BASE), "the login outranks the marker");
        let local = "http://127.0.0.1:8123/coding/v1";
        assert_eq!(base_url(Some(local), Some(GLOBAL_BASE), None).as_deref(), Some(local), "the environment outranks both");
        assert!(is_local(local) && !is_local(GLOBAL_BASE) && !is_local("http://127.0.0.1.evil.test/"));
        assert_eq!(base_url(Some("http://api.kimi.test/v1"), None, None), None, "a token is not sent in the clear");

        let home = Path::new("/h/.kimi-code");
        assert_eq!(credentials(home, None), home.join("credentials/kimi-code.json"));
        assert_eq!(credentials(home, Some("oauth/kimi-code-env-0123abcd")), home.join("credentials/kimi-code-env-0123abcd.json"));
        assert_eq!(credentials(home, Some("oauth/../../etc")), home.join("credentials/etc.json"));
        assert_eq!(credentials(home, Some("oauth/a.b")), home.join("credentials/kimi-code.json"));
    }

    fn sign_in(home: &Path, expires_at: i64) {
        std::fs::create_dir_all(home.join("credentials")).unwrap();
        let body = format!(r#"{{"access_token":"tok-1","expires_at":{expires_at}}}"#);
        std::fs::write(home.join("credentials/kimi-code.json"), body).unwrap();
    }

    #[test]
    fn homes_are_listed_once_and_an_expired_or_distant_account_is_not_asked() {
        let dir = marspot_term::test_env::TempDir::new("usage-kimi-homes");
        let home = std::fs::canonicalize(dir.path()).unwrap();
        assert!(homes(&home).is_empty());
        std::fs::create_dir_all(home.join(".kimi-code-profile-2")).unwrap();
        std::fs::create_dir_all(home.join(".kimi-code")).unwrap();
        let want = [
            Home { name: "Kimi 2".into(), path: home.join(".kimi-code-profile-2") },
            Home { name: "Kimi".into(), path: home.join(".kimi-code") },
        ];
        assert_eq!(homes(&home), want);
        std::fs::remove_dir(home.join(".kimi-code")).unwrap();
        std::os::unix::fs::symlink(home.join(".kimi-code-profile-2"), home.join(".kimi-code")).unwrap();
        assert_eq!(homes(&home), want[..1], "a default home that links to a profile is that profile");

        // no request leaves this test: the token has run out, and with
        // `local_only` the real service is not an address to ask
        let waits = home.join("kimi-waits.json");
        let held = vec![Account {
            name: "Kimi 2".into(),
            seven_day: Some(Window::new(0.4, Some(NOW + 3_600), NOW - 60)),
            last_read: NOW - 60,
            ..Account::default()
        }];
        let mut kimi = Kimi::default();
        sign_in(&home.join(".kimi-code-profile-2"), NOW - 1);
        let out = kimi.pass(&home, &waits, held.clone(), NOW, false);
        assert_eq!(out, held, "its last numbers stay");
        assert_eq!(kimi.states.get("Kimi 2"), Some(&State::TokenExpired));
        sign_in(&home.join(".kimi-code-profile-2"), NOW + 600);
        let out = kimi.pass(&home, &waits, held.clone(), NOW, true);
        assert_eq!(out, held);
        assert_eq!(kimi.states.get("Kimi 2"), Some(&State::TokenExpired), "not asked, so nothing new is known");
        assert!(!waits.exists());
    }
}
