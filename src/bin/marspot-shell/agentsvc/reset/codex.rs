//! A Codex claim: through a short-lived `codex app-server` under the
//! account's home, so the CLI does its own authentication.  Read the
//! limits and the credit list, find the credit the user confirmed by its
//! start and end, check that some limit it brings back has less than
//! 80% left, consume it with the request id as its idempotency key, and
//! read the limits again for what is left.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

use marspot_peer::json::{self, Value, obj};

use super::{Grant, Outcome, Res};
use crate::plugins::usage::codex::Replies;

/// A limit with this much of it left or more cannot be brought back.
const USABLE_BELOW_LEFT: f64 = 0.8;

fn request(id: &str, method: &str, params: Value) -> String {
    let mut line = obj([("jsonrpc", "2.0".into()), ("id", id.into()), ("method", method.into()), ("params", params)]).to_json();
    line.push('\n');
    line
}

const HELLO: &str = concat!(
    r#"{"jsonrpc":"2.0","id":"init","method":"initialize","params":{"clientInfo":{"name":"marspot","version":"1.0.0"}}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","id":"acct","method":"account/read","params":{}}"#,
    "\n",
);

/// The home of account `profile`: `~/.codex-profile-N`, `~/.codex` for 0.
fn home(profile: u8) -> PathBuf {
    let base = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let dir = if profile == 0 { base.join(".codex") } else { base.join(format!(".codex-profile-{profile}")) };
    std::fs::canonicalize(&dir).unwrap_or(dir)
}

pub(super) fn claim_profile(profile: u8, grant: &Grant, request_id: &str, deadline: Instant) -> Outcome {
    let home = home(profile);
    if crate::plugins::codex::installed_cli(&home).is_none() {
        return Outcome::of(Res::Unavailable, "no codex CLI is installed in that account's home");
    }
    let bin = home.join("packages/standalone/current/bin/codex");
    claim(&bin, &home, &format!("codex:{profile}"), grant, request_id, deadline)
}

/// One running app-server and what it has said.
struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Server {
    fn start(bin: &Path, home: &Path) -> Option<Server> {
        let mut child = Command::new(bin).arg("app-server").env("CODEX_HOME", home).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
        let (stdin, stdout) = (child.stdin.take()?, child.stdout.take()?);
        let (tx, lines) = channel();
        // ends when the server's stdout closes, which the kill on drop brings about
        let spawned = std::thread::Builder::new().name("reset-codex-read".into()).spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let mut s = Server { child, stdin, lines };
        (spawned.is_ok() && s.stdin.write_all(HELLO.as_bytes()).is_ok()).then_some(s)
    }

    /// Send `line` and wait for the answer whose id is `id`.
    fn ask(&mut self, line: &str, id: &str, deadline: Instant) -> Option<Value> {
        self.stdin.write_all(line.as_bytes()).ok()?;
        loop {
            let line = self.lines.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok()?;
            if let Ok(msg) = json::parse(&line)
                && msg.str_at("id") == Some(id)
            {
                return Some(msg);
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // `kill` signals this child's pid and no other
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `who` is `codex:<profile>`, the name the readings give the account.
pub(super) fn claim(bin: &Path, home: &Path, who: &str, grant: &Grant, request_id: &str, deadline: Instant) -> Outcome {
    let Some(mut server) = Server::start(bin, home) else { return Outcome::of(Res::Error, "the codex app-server could not be started") };
    let read = request("rl", "account/rateLimits/read", obj([]));
    let Some(limits) = server.ask(&read, "rl", deadline) else { return Outcome::of(Res::Error, "the app-server did not answer the limits request") };
    let credit = match pick(&limits, grant) {
        Ok(id) => id,
        Err(o) => return o,
    };
    if !below_usable(&limits) {
        return Outcome::of(Res::NotLimited, "every limit it brings back has 80% or more left");
    }
    let params = obj([("creditId", credit.as_str().into()), ("idempotencyKey", request_id.into())]);
    let Some(answer) = server.ask(&request("consume", "account/rateLimitResetCredit/consume", params), "consume", deadline) else {
        return Outcome::of(Res::Error, "the app-server did not answer the consume request; ask again with the same request id");
    };
    let mut o = consumed(&answer);
    let after = server.ask(&request("rl2", "account/rateLimits/read", obj([])), "rl2", deadline);
    let replies = Replies { account: None, limits: after };
    if let Some(r) = crate::plugins::usage::codex_resets::of_replies(&replies, super::unix_now()) {
        o.left_after = Some(r.left_total());
        o.observed_after = Some(r.to_value(who));
    }
    o
}

/// The id of the available credit that starts and ends as `grant` does.
pub(super) fn pick(limits: &Value, grant: &Grant) -> Result<String, Outcome> {
    if grant.starts_at.is_none() && grant.ends_at.is_none() {
        return Err(Outcome::of(Res::Unavailable, "the grant's start and end are not known, so no credit can be told apart"));
    }
    let credits = limits.at(&["result", "rateLimitResetCredits", "credits"]).map(Value::arr).unwrap_or_default();
    let same = |c: &&Value| {
        let at = |k: &str| c.get(k).and_then(Value::i64);
        grant.starts_at.is_none_or(|s| at("grantedAt") == Some(s)) && grant.ends_at.is_none_or(|e| at("expiresAt") == Some(e))
    };
    let Some(c) = credits.iter().find(|c| same(c) && c.str_at("status") == Some("available")).or_else(|| credits.iter().find(same)) else {
        return Err(Outcome::of(Res::Unavailable, "no credit of this account starts and ends as the confirmed one"));
    };
    match (c.str_at("status"), c.str_at("id")) {
        (Some("available"), Some(id)) => Ok(id.to_string()),
        (Some("redeemed"), _) => Err(Outcome::of(Res::AlreadyUsed, "that credit is already redeemed")),
        (status, _) => Err(Outcome::of(Res::Unavailable, &format!("that credit is {}", status.unwrap_or("of no known status")))),
    }
}

/// Whether some limit a credit brings back has less than 80% left: the
/// week, and on Plus the five-hour window too.
pub(super) fn below_usable(limits: &Value) -> bool {
    let Some(rate) = limits.at(&["result", "rateLimits"]) else { return false };
    let plus = rate.str_at("planType").is_some_and(|p| p.eq_ignore_ascii_case("plus"));
    ["primary", "secondary"].iter().filter_map(|k| rate.get(k)).any(|w| {
        let minutes = w.get("windowDurationMins").and_then(Value::i64);
        let covered = minutes == Some(10_080) || (plus && minutes == Some(300));
        let used = match w.get("usedPercent") {
            Some(Value::Num(n)) => *n / 100.0,
            _ => return false,
        };
        covered && 1.0 - used < USABLE_BELOW_LEFT
    })
}

/// The consume answer as a result.
pub(super) fn consumed(answer: &Value) -> Outcome {
    if let Some(msg) = answer.at(&["error", "message"]).and_then(Value::str) {
        return Outcome::of(Res::Error, msg);
    }
    let outcome = answer.at(&["result", "outcome"]).and_then(Value::str).unwrap_or("");
    let result = match outcome {
        "reset" => Res::Reset,
        "nothingToReset" => Res::NotLimited,
        "noCredit" => Res::NoCredit,
        "alreadyRedeemed" => Res::AlreadyUsed,
        _ => return Outcome::of(Res::Error, &format!("the app-server answered an outcome nobody here knows: {outcome:?}")),
    };
    Outcome::of(result, outcome)
}
