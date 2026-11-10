//! Codex accounts: one per `CODEX_HOME`, asked through a short-lived
//! `codex app-server` so the CLI does its own authentication and no
//! token is handled here.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use marspot_peer::json::{self, Value};

use super::feed::{own_window, Account, Credits, OwnWindow, Reading, Window};

const PROFILE_PREFIX: &str = ".codex-profile-";
/// How long one app-server gets to answer before it is stopped.
const ANSWER_WITHIN: Duration = Duration::from_secs(30);

const REQUESTS: &str = concat!(
    r#"{"jsonrpc":"2.0","id":"init","method":"initialize","params":{"clientInfo":{"name":"marspot","version":"1.0.0"}}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","id":"acct","method":"account/read","params":{}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","id":"rl","method":"account/rateLimits/read","params":{}}"#,
    "\n",
);

#[derive(Debug, PartialEq)]
pub(super) struct Home {
    pub name: String,
    pub path: PathBuf,
}

/// Every codex home under `home`, each once.  `~/.codex` is often a
/// link to a numbered profile; a home is known by where it really is,
/// and named after that.
pub(super) fn homes(home: &Path) -> Vec<Home> {
    let mut numbered: Vec<(u8, PathBuf)> = std::fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| Some((e.file_name().to_str()?.strip_prefix(PROFILE_PREFIX)?.parse().ok()?, e.path())))
        .collect();
    numbered.sort();
    let mut out: Vec<Home> = Vec::new();
    for candidate in numbered.into_iter().map(|(_, p)| p).chain([home.join(".codex")]) {
        let Ok(real) = std::fs::canonicalize(&candidate) else { continue };
        if !real.is_dir() || out.iter().any(|h| h.path == real) {
            continue;
        }
        let number = real.file_name().and_then(|n| n.to_str()?.strip_prefix(PROFILE_PREFIX)?.parse::<u8>().ok());
        let name = number.map_or("Codex".to_string(), |n| format!("Codex {n}"));
        out.push(Home { name, path: real });
    }
    out
}

/// The CLI installed in a home, if codex's own installer put one there.
fn cli(codex_home: &Path) -> Option<PathBuf> {
    crate::plugins::codex::installed_cli(codex_home)?;
    Some(codex_home.join("packages/standalone/current/bin/codex"))
}

/// The two answers wanted from one app-server, as whole messages.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Replies {
    pub account: Option<Value>,
    pub limits: Option<Value>,
}

impl Replies {
    /// Keep a line if it is one of the two answers; the server also
    /// sends notifications and the answer to `initialize`.
    pub(super) fn take(&mut self, line: &str) {
        let Ok(msg) = json::parse(line) else { return };
        match msg.str_at("id") {
            Some("acct") => self.account = Some(msg),
            Some("rl") => self.limits = Some(msg),
            _ => {}
        }
    }

    fn complete(&self) -> bool {
        self.account.is_some() && self.limits.is_some()
    }
}

/// Run one app-server under `codex_home`, ask it, and stop it.
fn ask(bin: &Path, codex_home: &Path) -> Replies {
    let mut replies = Replies::default();
    let spawned = Command::new(bin)
        .arg("app-server")
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = spawned else { return replies };
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return replies;
    };
    let (tx, rx) = mpsc::channel::<String>();
    // Detached: it ends when the server's stdout closes, which the kill
    // below brings about.
    let reader = std::thread::Builder::new().name("usage-codex-read".into()).spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    if reader.is_ok() && stdin.write_all(REQUESTS.as_bytes()).is_ok() {
        let deadline = Instant::now() + ANSWER_WITHIN;
        while !replies.complete() {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(line) => replies.take(&line),
                Err(_) => break,
            }
        }
    }
    // `kill` signals this child's pid and no other
    let _ = child.kill();
    let _ = child.wait();
    replies
}

/// What one home's answers came to.
#[derive(Debug, PartialEq)]
pub(super) enum Outcome {
    Read { email: String, tier: String, reading: Reading },
    /// Nobody is logged in there, or the login is refused.
    AuthError,
    Nothing,
}

fn f64_at(v: &Value, key: &str) -> Option<f64> {
    match v.get(key) {
        Some(Value::Num(n)) => Some(*n),
        _ => None,
    }
}

fn present<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key).filter(|x| **x != Value::Null)
}

/// Is this message an error about the login, as opposed to any other
/// failure?  The server has no error code for it, only the wording.
fn auth_failed(msg: &Value) -> bool {
    let Some(text) = msg.at(&["error", "message"]).and_then(Value::str) else {
        return false;
    };
    let text = text.to_ascii_lowercase();
    ["auth", "log in", "login", "401"].iter().any(|w| text.contains(w))
}

pub(super) fn outcome(replies: &Replies, now: i64) -> Outcome {
    let (Some(account), Some(limits)) = (&replies.account, &replies.limits) else {
        return Outcome::Nothing;
    };
    if auth_failed(account) || auth_failed(limits) {
        return Outcome::AuthError;
    }
    let Some(result) = account.get("result") else {
        return Outcome::Nothing;
    };
    let Some(email) = result.at(&["account", "email"]).and_then(Value::str) else {
        return Outcome::AuthError;
    };
    let Some(rate) = limits.at(&["result", "rateLimits"]) else {
        return Outcome::Nothing;
    };
    let mut reading = Reading {
        limit_reached: present(rate, "rateLimitReachedType").is_some(),
        credits: present(rate, "credits").map(credits_of),
        ..Reading::default()
    };
    for key in ["primary", "secondary"] {
        let Some(w) = present(rate, key) else { continue };
        let (Some(percent), Some(minutes)) = (f64_at(w, "usedPercent"), w.get("windowDurationMins").and_then(Value::i64))
        else {
            continue;
        };
        let window = Window::new(percent / 100.0, w.get("resetsAt").and_then(Value::i64), now);
        match own_window(minutes) {
            Some(OwnWindow::FiveHour) => reading.five_hour = Some(window),
            Some(OwnWindow::SevenDay) => reading.seven_day = Some(window),
            None => {}
        }
    }
    let tier = result.at(&["account", "planType"]).and_then(Value::str).or(rate.str_at("planType"));
    Outcome::Read { email: email.to_string(), tier: tier.unwrap_or_default().to_string(), reading }
}

fn credits_of(c: &Value) -> Credits {
    Credits {
        enabled: c.get("hasCredits").and_then(Value::bool),
        unlimited: c.get("unlimited").and_then(Value::bool),
        balance: match c.get("balance") {
            Some(Value::Str(s)) => s.parse().ok(),
            Some(Value::Num(n)) => Some(*n),
            _ => None,
        },
        ..Credits::default()
    }
}

/// An auth error marks a home that has been read before; one that
/// never was stays out of the feed, since there is nothing to say whose
/// it is.
pub(super) fn apply(account: &mut Account, outcome: Outcome, now: i64) {
    match outcome {
        Outcome::Read { email, tier, reading } => {
            account.email = email;
            account.tier = tier;
            account.absorb(reading, now);
        }
        Outcome::AuthError => account.auth_error = account.has_reading(),
        Outcome::Nothing => {}
    }
}

/// One pass over every codex home under `home`.  A home whose CLI is
/// not installed is not asked and keeps what it had.
pub(super) fn pass(home: &Path, held: Vec<Account>, now: i64) -> Vec<Account> {
    let mut held = held;
    let mut out = Vec::new();
    for h in homes(home) {
        let mut account = match held.iter().position(|a| a.name == h.name) {
            Some(i) => held.swap_remove(i),
            None => Account { name: h.name.clone(), ..Account::default() },
        };
        if let Some(bin) = cli(&h.path) {
            apply(&mut account, outcome(&ask(&bin, &h.path), now), now);
        }
        account.expire(now);
        out.push(account);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_290_114;

    /// What `codex app-server` 0.167.0 printed for a logged-in account
    /// on 2026-11-10, trimmed of the reset-credit list.
    const LINES: &str = concat!(
        r#"{"id":"init","result":{"userAgent":"marspot/0.167.0","codexHome":"/Users/x/.codex-profile-1"}}"#,
        "\n",
        r#"{"method":"remoteControl/status/changed","params":{"status":"disabled"},"emittedAtMs":1794290562246}"#,
        "\n",
        r#"{"id":"rl","result":{"ordinaryUsageAllowed":false,"rateLimits":{"limitId":"codex","limitName":null,"#,
        r#""primary":{"usedPercent":100,"windowDurationMins":10080,"resetsAt":1794427376},"secondary":null,"#,
        r#""credits":{"hasCredits":true,"unlimited":false,"balance":"60460.5367250000"},"individualLimit":null,"#,
        r#""planType":"pro","rateLimitReachedType":"rate_limit_reached"},"accountId":"6dc9"}}"#,
        "\n",
        r#"{"method":"account/updated","params":{"authMode":"chatgpt","planType":"pro"}}"#,
        "\n",
        r#"{"id":"acct","result":{"account":{"type":"chatgpt","email":"ada@example.jp","planType":"pro"},"#,
        r#""requiresOpenaiAuth":true}}"#,
        "\n",
    );

    fn replies(lines: &str) -> Replies {
        let mut r = Replies::default();
        lines.lines().for_each(|l| r.take(l));
        r
    }

    #[test]
    fn only_the_two_answers_are_kept_from_what_the_server_prints() {
        let mut r = Replies::default();
        let mut lines = LINES.lines();
        for _ in 0..3 {
            r.take(lines.next().unwrap());
        }
        assert!(r.account.is_none() && r.limits.is_some() && !r.complete());
        lines.for_each(|l| r.take(l));
        assert!(r.complete());
        r.take("not json");
        assert_eq!(r.account.as_ref().and_then(|a| a.str_at("id")), Some("acct"));
    }

    #[test]
    fn the_answers_become_the_accounts_windows_plan_and_credits() {
        let Outcome::Read { email, tier, reading } = outcome(&replies(LINES), NOW) else {
            panic!("a logged-in account reads");
        };
        assert_eq!((email.as_str(), tier.as_str()), ("ada@example.jp", "pro"));
        assert_eq!(reading.five_hour, None, "no 300-minute window was reported");
        assert_eq!(reading.seven_day, Some(Window::new(1.0, Some(1_794_427_376), NOW)));
        assert!(reading.limit_reached, "rateLimitReachedType is set");
        let c = reading.credits.expect("credits");
        assert_eq!((c.enabled, c.unlimited, c.balance), (Some(true), Some(false), Some(60_460.536_725)));
    }

    #[test]
    fn each_window_is_placed_by_its_length() {
        let lines = LINES.replace(
            r#""secondary":null"#,
            r#""secondary":{"usedPercent":12.5,"windowDurationMins":300,"resetsAt":1794300000}"#,
        );
        let lines = lines.replace(r#""rateLimitReachedType":"rate_limit_reached""#, r#""rateLimitReachedType":null"#);
        let lines = lines.replace(r#""windowDurationMins":10080"#, r#""windowDurationMins":10080.0"#);
        let Outcome::Read { reading, .. } = outcome(&replies(&lines), NOW) else {
            panic!("reads");
        };
        assert_eq!(reading.five_hour, Some(Window::new(0.125, Some(1_794_300_000), NOW)));
        assert_eq!(reading.seven_day.map(|w| w.util), Some(1.0));
        assert!(!reading.limit_reached);

        let odd = LINES.replace(r#""windowDurationMins":10080"#, r#""windowDurationMins":1440"#);
        let Outcome::Read { reading, .. } = outcome(&replies(&odd), NOW) else {
            panic!("reads");
        };
        assert_eq!((reading.five_hour, reading.seven_day), (None, None), "a daily window is neither");
    }

    #[test]
    fn no_address_or_a_login_error_is_an_auth_error_and_silence_is_nothing() {
        let logged_out = LINES.replace(
            r#"{"type":"chatgpt","email":"ada@example.jp","planType":"pro"}"#,
            "null",
        );
        assert_eq!(outcome(&replies(&logged_out), NOW), Outcome::AuthError);

        let refused = concat!(
            r#"{"id":"acct","result":{"account":{"email":"ada@example.jp"}}}"#,
            "\n",
            r#"{"id":"rl","error":{"code":-32603,"message":"failed to fetch codex rate limits: 401 Unauthorized"}}"#,
        );
        assert_eq!(outcome(&replies(refused), NOW), Outcome::AuthError);

        let broken = refused.replace("401 Unauthorized", "connection reset");
        assert_eq!(outcome(&replies(&broken), NOW), Outcome::Nothing, "not every error is about the login");
        let half = LINES.lines().take(3).collect::<Vec<_>>().join("\n");
        assert_eq!(outcome(&replies(&half), NOW), Outcome::Nothing, "the server did not answer in time");
        assert_eq!(outcome(&Replies::default(), NOW), Outcome::Nothing);
    }

    #[test]
    fn an_auth_error_marks_only_a_home_that_was_read_before() {
        let mut never = Account { name: "Codex 2".into(), ..Account::default() };
        apply(&mut never, Outcome::AuthError, NOW);
        assert!(!never.auth_error && !never.has_reading(), "nothing known, no row");

        let mut known = Account { name: "Codex 1".into(), ..Account::default() };
        apply(&mut known, outcome(&replies(LINES), NOW), NOW);
        assert_eq!((known.email.as_str(), known.tier.as_str(), known.status()), ("ada@example.jp", "pro", "rejected"));
        apply(&mut known, Outcome::Nothing, NOW + 300);
        assert_eq!(known.status(), "rejected", "a pass with no answer changes nothing");
        apply(&mut known, Outcome::AuthError, NOW + 600);
        assert_eq!(known.status(), "auth_error");
        assert_eq!(known.seven_day.as_ref().map(|w| w.util), Some(1.0), "its last numbers stay");
    }

    #[test]
    fn a_home_reached_by_a_link_is_listed_once_under_its_real_name() {
        let dir = marspot_term::test_env::TempDir::new("usage-codex-homes");
        let home = std::fs::canonicalize(dir.path()).unwrap();
        assert!(homes(&home).is_empty());
        for n in [2, 1] {
            std::fs::create_dir_all(home.join(format!(".codex-profile-{n}"))).unwrap();
        }
        std::fs::write(home.join(".codex-profile-9"), "a file, not a home").unwrap();
        std::os::unix::fs::symlink(home.join(".codex-profile-1"), home.join(".codex")).unwrap();
        let want = [1, 2].map(|n| Home { name: format!("Codex {n}"), path: home.join(format!(".codex-profile-{n}")) });
        assert_eq!(homes(&home), want);

        std::fs::remove_file(home.join(".codex")).unwrap();
        std::fs::create_dir(home.join(".codex")).unwrap();
        let got = homes(&home);
        assert_eq!(got.len(), 3);
        assert_eq!(got[2], Home { name: "Codex".into(), path: home.join(".codex") });
    }
}
