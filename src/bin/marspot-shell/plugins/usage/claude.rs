//! Claude accounts: one per config directory, read from the usage
//! endpoint the CLI's own `/usage` screen calls.
//!
//! The login is read and never refreshed.  A refresh from outside the
//! CLI invalidates the token the CLI holds, which logs the account out.
//! Nothing here sends a message either: that is a paid call, and this
//! runs unattended every few minutes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use marspot_peer::json::{self, Value};

use super::feed::{Account, Credits, ModelWindow, Reading, Window, WEEK_MINUTES};
use super::http::{self, Response};
use super::sha256;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// How long to stay away after a 429 that does not say.
const DEFAULT_RETRY_SECS: i64 = 300;

/// One config directory and the account logged in there.
#[derive(Debug, PartialEq)]
pub(super) struct ConfigDir {
    pub name: String,
    pub path: PathBuf,
    pub email: String,
}

/// Every logged-in config directory under `home`, one per account.  A
/// numbered profile comes before `~/.claude`, so an account logged in
/// at both is named after the profile.
pub(super) fn config_dirs(home: &Path) -> Vec<ConfigDir> {
    use crate::plugins::claudecode::{discover_profiles_in, email_in};
    let mut out: Vec<ConfigDir> = Vec::new();
    let mut add = |name: String, path: PathBuf, email: Option<String>| {
        if let Some(email) = email
            && !out.iter().any(|d| d.email == email)
        {
            out.push(ConfigDir { name, path, email });
        }
    };
    for n in discover_profiles_in(home) {
        let path = home.join(format!(".claude-profile-{n}"));
        let email = email_in(&path.join(".claude.json"));
        add(format!("Claude {n}"), path, email);
    }
    let default = home.join(".claude");
    // without CLAUDE_CONFIG_DIR the CLI keeps this file beside the
    // directory, not in it
    let email = email_in(&default.join(".claude.json")).or_else(|| email_in(&home.join(".claude.json")));
    if default.is_dir() {
        add("Claude".into(), default, email);
    }
    out
}

/// The keychain entry Claude Code keeps a config directory's login in.
pub(super) fn keychain_service(dir: &Path, home: &Path) -> String {
    const BASE: &str = "Claude Code-credentials";
    if dir == home.join(".claude") {
        return BASE.into();
    }
    let digest = sha256::digest(dir.to_string_lossy().as_bytes());
    format!("{BASE}-{}", sha256::hex(&digest[..4]))
}

/// Not `Debug`: it holds the token, and nothing may print one.
pub(super) struct Login {
    pub token: String,
    /// Unix milliseconds.
    pub expires_at: Option<i64>,
    pub tier: Option<String>,
}

impl Login {
    fn expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|ms| ms / 1000 <= now)
    }
}

pub(super) fn parse_login(text: &str) -> Option<Login> {
    let v = json::parse(text.trim()).ok()?;
    let oauth = v.get("claudeAiOauth")?;
    Some(Login {
        token: oauth.str_at("accessToken")?.to_string(),
        expires_at: oauth.get("expiresAt").and_then(Value::i64),
        tier: oauth.str_at("rateLimitTier").map(str::to_string),
    })
}

/// The login of one config directory: the keychain, then the file the
/// CLI writes where it has no keychain.
fn read_login(dir: &Path, home: &Path) -> Option<Login> {
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", &keychain_service(dir, home), "-w"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok();
    out.filter(|o| o.status.success())
        .and_then(|o| parse_login(&String::from_utf8_lossy(&o.stdout)))
        .or_else(|| parse_login(&std::fs::read_to_string(dir.join(".credentials.json")).ok()?))
}

fn f64_at(v: &Value, key: &str) -> Option<f64> {
    match v.get(key) {
        Some(Value::Num(n)) => Some(*n),
        _ => None,
    }
}

/// The windows and credits in a usage body.  Only `limits[]` is read:
/// the body's other fields are older spellings of the same windows.
pub(super) fn parse_usage(body: &str, now: i64) -> Option<Reading> {
    let v = json::parse(body).ok()?;
    let Some(Value::Arr(limits)) = v.get("limits") else {
        return None;
    };
    let mut r = Reading { credits: credits_of(&v), ..Reading::default() };
    for l in limits {
        let Some(percent) = f64_at(l, "percent") else { continue };
        let window = Window {
            util: percent / 100.0,
            reset: l.str_at("resets_at").and_then(|s| marspot::cc_usage::parse_iso_utc(s.to_string())),
            severity: l.str_at("severity").map(str::to_string),
            is_active: l.get("is_active").and_then(Value::bool),
            seen: now,
        };
        match l.str_at("kind") {
            Some("session") => r.five_hour = Some(window),
            Some("weekly_all") => r.seven_day = Some(window),
            Some("weekly_scoped") => {
                if let Some(label) = l.at(&["scope", "model", "display_name"]).and_then(Value::str) {
                    r.models.push(ModelWindow { label: label.to_string(), minutes: WEEK_MINUTES, window });
                }
            }
            _ => {}
        }
    }
    Some(r)
}

fn credits_of(v: &Value) -> Option<Credits> {
    let present = |key: &str| v.get(key).filter(|x| **x != Value::Null);
    let (extra, spend) = (present("extra_usage"), present("spend"));
    if extra.is_none() && spend.is_none() {
        return None;
    }
    let flag = |o: Option<&Value>, key: &str| o?.get(key)?.bool();
    let reason = |o: Option<&Value>| Some(o?.str_at("disabled_reason")?.to_string());
    let money = |key: &str| {
        let m = spend?.get(key)?;
        let exponent = m.get("exponent").and_then(Value::i64).unwrap_or(2);
        Some(f64_at(m, "amount_minor")? / 10f64.powi(exponent as i32))
    };
    let (used, limit) = (money("used"), money("limit"));
    Some(Credits {
        enabled: flag(extra, "is_enabled").or(flag(spend, "enabled")),
        unlimited: None,
        balance: used.zip(limit).map(|(u, l)| l - u),
        used,
        limit,
        utilization: spend.and_then(|s| f64_at(s, "percent")).map(|p| p / 100.0),
        disabled_reason: reason(spend)
            .or_else(|| reason(extra))
            .or_else(|| (flag(extra, "user_disabled") == Some(true)).then(|| "user_disabled".into()))
            .or_else(|| (flag(extra, "credits_ever_enabled") == Some(false)).then(|| "never_enabled".into())),
    })
}

/// Until when each account's usage endpoint is not to be called, by
/// address.  Kept in a file so a restart does not walk straight back
/// into a refusal.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Waits(BTreeMap<String, i64>);

impl Waits {
    pub(super) fn parse(text: &str) -> Waits {
        let Ok(Value::Obj(kv)) = json::parse(text) else {
            return Waits::default();
        };
        Waits(kv.into_iter().filter_map(|(email, until)| Some((email, until.i64()?))).collect())
    }

    pub(super) fn to_json(&self) -> String {
        Value::Obj(self.0.iter().map(|(email, until)| (email.clone(), Value::from(*until))).collect()).to_json()
    }

    pub(super) fn blocked(&self, email: &str, now: i64) -> bool {
        self.0.get(email).is_some_and(|until| now < *until)
    }

    /// Forget the waits that are over, so the file holds only accounts
    /// currently refused.
    pub(super) fn prune(&mut self, now: i64) {
        self.0.retain(|_, until| now < *until);
    }
}

/// What one request to the usage endpoint came to.
#[derive(Debug, PartialEq)]
// one of these per account per pass, held for a moment: boxing the
// reading would add an allocation to save nothing
#[allow(clippy::large_enum_variant)]
pub(super) enum Outcome {
    Read(Reading),
    /// Refused for now; do not ask again before this unix time.
    Wait(i64),
    AuthError,
    /// No answer, or one that says nothing about the account.
    Nothing,
}

pub(super) fn outcome(resp: Option<&Response>, now: i64) -> Outcome {
    match resp {
        Some(r) if r.status == 200 => parse_usage(&r.body, now).map_or(Outcome::Nothing, Outcome::Read),
        Some(r) if r.status == 429 => Outcome::Wait(now + r.retry_after.unwrap_or(DEFAULT_RETRY_SECS)),
        Some(r) if r.status == 401 || r.status == 403 => Outcome::AuthError,
        _ => Outcome::Nothing,
    }
}

pub(super) fn apply(account: &mut Account, waits: &mut Waits, outcome: Outcome, now: i64) {
    match outcome {
        Outcome::Read(reading) => account.absorb(reading, now),
        Outcome::Wait(until) => {
            waits.0.insert(account.email.clone(), until);
        }
        Outcome::AuthError => account.auth_error = true,
        Outcome::Nothing => {}
    }
}

/// Ask the endpoint about one account, unless its login has run out or
/// the endpoint said to stay away.
fn poll(dir: &ConfigDir, home: &Path, account: &mut Account, waits: &Waits, now: i64) -> Outcome {
    let Some(login) = read_login(&dir.path, home) else {
        return Outcome::Nothing;
    };
    if let Some(tier) = &login.tier {
        account.tier = tier.clone();
    }
    if login.expired(now) || waits.blocked(&dir.email, now) {
        return Outcome::Nothing;
    }
    outcome(http::get(USAGE_URL, &login.token).as_ref(), now)
}

/// One pass over every Claude account under `home`.  `held` is what
/// earlier passes left; the result holds the accounts that still exist.
pub(super) fn pass(home: &Path, waits_file: &Path, held: Vec<Account>, now: i64) -> Vec<Account> {
    let mut waits = Waits::parse(&std::fs::read_to_string(waits_file).unwrap_or_default());
    let before = waits.to_json();
    waits.prune(now);
    let mut held = held;
    let mut out = Vec::new();
    for dir in config_dirs(home) {
        let mut account = match held.iter().position(|a| a.email == dir.email) {
            Some(i) => held.swap_remove(i),
            None => Account { email: dir.email.clone(), ..Account::default() },
        };
        account.name = dir.name.clone();
        let outcome = poll(&dir, home, &mut account, &waits, now);
        apply(&mut account, &mut waits, outcome, now);
        account.expire(now);
        out.push(account);
    }
    if waits.to_json() != before {
        let _ = super::feed::write_atomic(waits_file, &waits.to_json());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_290_114;

    /// A body as the endpoint returned it on 2026-08-01, with the older
    /// spellings of the same windows that are not to be read.
    const BODY: &str = r#"{
      "five_hour": {"utilization": 6.0, "resets_at": "2026-08-01T00:20:00+00:00"},
      "seven_day": {"utilization": 62.0, "resets_at": "2026-08-02T05:00:00+00:00"},
      "seven_day_opus": null,
      "seven_day_breakdown": {"claude_code": 0.9},
      "extra_usage": {"is_enabled": false, "user_disabled": true, "credits_ever_enabled": true},
      "limits": [
        {"kind": "session", "percent": 6, "severity": "normal", "resets_at": "2026-08-01T00:20:00+00:00", "scope": null, "is_active": false},
        {"kind": "weekly_all", "percent": 62, "severity": "normal", "resets_at": "2026-08-02T05:00:00.123456Z", "scope": null, "is_active": false},
        {"kind": "weekly_scoped", "percent": 82, "severity": "warning", "resets_at": "2026-08-02T05:00:00+00:00",
         "scope": {"model": {"id": null, "display_name": "Fable"}}, "is_active": true},
        {"kind": "something_new", "percent": 99, "resets_at": null}
      ],
      "spend": {"enabled": false, "percent": 25, "used": {"amount_minor": 5000, "currency": "USD", "exponent": 2},
                "limit": {"amount_minor": 20000, "currency": "USD"}}
    }"#;

    #[test]
    fn a_profiles_keychain_entry_is_named_after_a_digest_of_its_path() {
        let home = Path::new("/Users/x");
        assert_eq!(keychain_service(&home.join(".claude"), home), "Claude Code-credentials");
        // printf '%s' /Users/x/.claude-profile-2 | shasum -a 256
        //   73ef4ed71fd07226940bbd796f72b1088bd0f49ba944cc4afa4e9b7b6a89f641
        assert_eq!(
            keychain_service(&home.join(".claude-profile-2"), home),
            "Claude Code-credentials-73ef4ed7"
        );
    }

    #[test]
    fn a_usage_body_becomes_the_5h_7d_and_per_model_windows() {
        let r = parse_usage(BODY, NOW).expect("a body with limits");
        let five = r.five_hour.expect("session is the 5h window");
        assert_eq!((five.util, five.reset, five.seen), (0.06, Some(1_785_543_600), NOW));
        assert_eq!((five.severity.as_deref(), five.is_active), (Some("normal"), Some(false)));
        let seven = r.seven_day.expect("weekly_all is the 7d window");
        assert_eq!((seven.util, seven.reset), (0.62, Some(1_785_646_800)));
        assert_eq!(r.models.len(), 1, "a kind nobody knows is not a window");
        let fable = &r.models[0];
        assert_eq!((fable.label.as_str(), fable.minutes), ("Fable", 10_080));
        assert_eq!((fable.window.util, fable.window.reset), (0.82, Some(1_785_646_800)));
        assert_eq!((fable.window.severity.as_deref(), fable.window.is_active), (Some("warning"), Some(true)));
        assert!(!r.limit_reached);
    }

    #[test]
    fn spend_and_extra_usage_become_credits() {
        let c = parse_usage(BODY, NOW).unwrap().credits.expect("credits");
        assert_eq!(c.enabled, Some(false), "extra_usage.is_enabled comes first");
        assert_eq!((c.used, c.limit, c.balance), (Some(50.0), Some(200.0), Some(150.0)));
        assert_eq!(c.utilization, Some(0.25));
        assert_eq!(c.disabled_reason.as_deref(), Some("user_disabled"));
        assert_eq!(c.unlimited, None);

        let never = r#"{"limits":[],"extra_usage":{"credits_ever_enabled":false},
            "spend":{"enabled":true,"used":{"amount_minor":1234,"exponent":3},"limit":null}}"#;
        let c = parse_usage(never, NOW).unwrap().credits.unwrap();
        assert_eq!(c.enabled, Some(true), "spend.enabled when extra_usage does not say");
        assert_eq!((c.used, c.limit, c.balance), (Some(1.234), None, None));
        assert_eq!(c.disabled_reason.as_deref(), Some("never_enabled"));

        let said = r#"{"limits":[],"spend":{"disabled_reason":"out_of_credits"},
            "extra_usage":{"disabled_reason":"other","user_disabled":true}}"#;
        let c = parse_usage(said, NOW).unwrap().credits.unwrap();
        assert_eq!(c.disabled_reason.as_deref(), Some("out_of_credits"), "spend's reason comes first");
        assert_eq!(parse_usage(r#"{"limits":[]}"#, NOW).unwrap().credits, None);
    }

    #[test]
    fn a_body_without_limits_is_no_reading() {
        assert_eq!(parse_usage(r#"{"five_hour":{"utilization":1.0}}"#, NOW), None);
        assert_eq!(parse_usage(r#"{"limits":null}"#, NOW), None);
        assert_eq!(parse_usage("<html>", NOW), None);
    }

    #[test]
    fn a_login_is_read_from_the_keychain_json() {
        let text = r#"{"claudeAiOauth":{"accessToken":"tok","refreshToken":"r","expiresAt":1794290115000,
            "scopes":["user:inference"],"subscriptionType":"max","rateLimitTier":"default_claude_max_20x"}}
"#;
        let login = parse_login(text).expect("a login");
        assert_eq!(login.token, "tok");
        assert_eq!(login.tier.as_deref(), Some("default_claude_max_20x"));
        assert!(!login.expired(NOW), "one second left");
        assert!(login.expired(NOW + 1), "a token at its expiry is not sent");
        assert!(parse_login(r#"{"mcpOAuth":{}}"#).is_none());
        assert!(parse_login("").is_none());
    }

    fn answer(status: u16, retry_after: Option<i64>, body: &str) -> Response {
        Response { status, retry_after, body: body.into() }
    }

    #[test]
    fn each_status_is_read_for_what_it_says_about_the_account() {
        assert!(matches!(outcome(Some(&answer(200, None, BODY)), NOW), Outcome::Read(_)));
        assert_eq!(outcome(Some(&answer(200, None, "{}")), NOW), Outcome::Nothing);
        assert_eq!(outcome(Some(&answer(429, Some(120), "")), NOW), Outcome::Wait(NOW + 120));
        assert_eq!(outcome(Some(&answer(429, None, "")), NOW), Outcome::Wait(NOW + 300));
        assert_eq!(outcome(Some(&answer(401, None, "")), NOW), Outcome::AuthError);
        assert_eq!(outcome(Some(&answer(403, None, "")), NOW), Outcome::AuthError);
        assert_eq!(outcome(Some(&answer(500, None, BODY)), NOW), Outcome::Nothing);
        assert_eq!(outcome(None, NOW), Outcome::Nothing);
    }

    #[test]
    fn a_429_keeps_the_endpoint_uncalled_until_it_said_and_across_a_restart() {
        let mut account = Account { email: "a@b".into(), ..Account::default() };
        let mut waits = Waits::default();
        assert!(!waits.blocked("a@b", NOW));
        apply(&mut account, &mut waits, Outcome::Wait(NOW + 120), NOW);
        assert!(waits.blocked("a@b", NOW + 119));
        assert!(!waits.blocked("a@b", NOW + 120));
        assert!(!waits.blocked("c@d", NOW), "the refusal was this account's");
        assert!(!account.has_reading() && !account.auth_error, "a refusal says nothing about the account");

        let mut reloaded = Waits::parse(&waits.to_json());
        assert_eq!(reloaded, waits);
        assert!(reloaded.blocked("a@b", NOW + 119));
        reloaded.prune(NOW + 120);
        assert_eq!(reloaded.to_json(), "{}");
        assert_eq!(Waits::parse("not json"), Waits::default());
    }

    #[test]
    fn an_auth_failure_marks_the_account_until_it_reads_again() {
        let mut account = Account { email: "a@b".into(), ..Account::default() };
        let mut waits = Waits::default();
        apply(&mut account, &mut waits, outcome(Some(&answer(200, None, BODY)), NOW), NOW);
        assert_eq!(account.status(), "allowed");
        apply(&mut account, &mut waits, Outcome::AuthError, NOW + 300);
        assert_eq!(account.status(), "auth_error");
        assert_eq!(account.seven_day.as_ref().map(|w| w.util), Some(0.62), "its last numbers stay");
        apply(&mut account, &mut waits, Outcome::Nothing, NOW + 600);
        assert_eq!(account.status(), "auth_error");
        apply(&mut account, &mut waits, outcome(Some(&answer(200, None, BODY)), NOW + 900), NOW + 900);
        assert_eq!(account.status(), "allowed");
    }

    fn login_as(dir: &Path, email: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let config = format!("{{\n  \"oauthAccount\": {{\n    \"emailAddress\": \"{email}\"\n  }}\n}}\n");
        std::fs::write(dir.join(".claude.json"), config).unwrap();
    }

    #[test]
    fn config_dirs_are_listed_once_per_account_and_named_after_the_profile() {
        let home = marspot_term::test_env::TempDir::new("usage-claude-dirs");
        let home = home.path();
        login_as(&home.join(".claude-profile-2"), "two@x");
        login_as(&home.join(".claude-profile-1"), "one@x");
        std::fs::create_dir_all(home.join(".claude-profile-3")).unwrap();
        login_as(&home.join(".claude"), "one@x");
        let got: Vec<(String, String)> = config_dirs(home).into_iter().map(|d| (d.name, d.email)).collect();
        let want = [("Claude 1", "one@x"), ("Claude 2", "two@x")].map(|(n, e)| (n.to_string(), e.to_string()));
        assert_eq!(got, want, "a profile with no login is skipped; ~/.claude repeats profile 1");

        login_as(&home.join(".claude"), "zero@x");
        let dirs = config_dirs(home);
        assert_eq!(dirs.len(), 3);
        assert_eq!(dirs[2], ConfigDir { name: "Claude".into(), path: home.join(".claude"), email: "zero@x".into() });

        // the CLI's default layout: the config sits beside the directory
        std::fs::remove_file(home.join(".claude/.claude.json")).unwrap();
        login_as(home, "beside@x");
        assert_eq!(config_dirs(home)[2].email, "beside@x");
    }
}
