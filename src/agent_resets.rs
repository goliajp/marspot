//! The stored rate-limit resets of each agent account, as last read.
//!
//! The shell's usage collector writes them to `usage/resets.json`
//! under the state root, one `resets` block per account in the shape
//! devops reads; the accounts panel and the allocation read them back
//! from there.  The file is a derivation: the readings themselves are
//! appended to `usage/resets/<date>.tsv` as facts, and the last row of
//! each account there gives the same block again.
//!
//! No real grant or credit id and no address is ever written: a grant
//! is known by `<agent>:<profile>:g<n>`.

use std::path::PathBuf;

use marspot_peer::json::{self, Value, obj};

mod claim;
mod claimed;
mod lines;
pub use claim::{ARMED_FOR_S, Armed, ResetButtons, ResetLine, armed_text, claim_path_on, claimable};
pub use claimed::{Claimed, claimed_text, claims_dir, last_claims};
pub use lines::{MAX_LINES, line_count, lines};

/// How a reading of an account's resets ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The list was read; it may be empty.
    Read,
    /// The client has no way to list them (a hidden command switched
    /// off for this account).
    CommandMissing,
    /// The provider says the account cannot have any.
    Ineligible,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Read => "read",
            Outcome::CommandMissing => "command_not_available",
            Outcome::Ineligible => "ineligible",
        }
    }

    pub fn parse(s: &str) -> Option<Outcome> {
        [Outcome::Read, Outcome::CommandMissing, Outcome::Ineligible].into_iter().find(|o| o.as_str() == s)
    }
}

/// One stored reset, or one batch of them that share their terms.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Grant {
    /// `full`, `weekly`, `session`, `fable`, `codex_full`, or a word
    /// nobody here knows, kept as given.
    pub kind: String,
    /// The windows it brings back: `five_hour`, `seven_day`,
    /// `seven_day_overage_included`.
    pub restores: Vec<String>,
    pub gives_back_weekly: bool,
    /// Using it moves the week's renewal to a week after the use.
    pub moves_weekly_reset: bool,
    pub left: Option<u32>,
    pub total: Option<u32>,
    pub usable_now: Option<bool>,
    pub use_requires_limit: Option<bool>,
    pub unavailable_reason: Option<String>,
    /// Unix seconds.
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    /// The provider names this one as the next it would use.
    pub is_next: bool,
}

/// One account's resets as one reading saw them.
#[derive(Clone, Debug, PartialEq)]
pub struct Resets {
    /// Unix seconds.
    pub observed_at: i64,
    /// `app_server`, `usage_api`, `limit_reset_screen` or `devops_feed`.
    pub source: String,
    pub outcome: Outcome,
    pub eligible: Option<bool>,
    pub ineligible_reason: Option<String>,
    pub grants: Vec<Grant>,
}

impl Resets {
    /// An account that has no resets of any kind, as Gemini and Kimi.
    pub fn none(observed_at: i64, source: &str) -> Resets {
        Resets { observed_at, source: source.into(), outcome: Outcome::Read, eligible: None, ineligible_reason: None, grants: Vec::new() }
    }

    /// Resets still left, over every grant.
    pub fn left_total(&self) -> u32 {
        self.grants.iter().filter_map(|g| g.left).sum()
    }

    /// The earliest end of a grant with any left.
    pub fn next_expiry(&self) -> Option<i64> {
        self.grants.iter().filter(|g| g.left != Some(0)).filter_map(|g| g.ends_at).min()
    }

    /// The same without the grants that ended by `now`.
    pub fn live(mut self, now: i64) -> Resets {
        self.grants.retain(|g| g.ends_at.is_none_or(|e| e > now));
        self
    }

    /// Whether two readings say the same, whenever they were made.
    pub fn same_as(&self, other: &Resets) -> bool {
        Resets { observed_at: other.observed_at, ..self.clone() } == *other
    }

    /// The block devops reads; `who` is `<agent>:<profile>`.
    pub fn to_value(&self, who: &str) -> Value {
        let grants: Vec<Value> = self.grants.iter().enumerate().map(|(i, g)| grant_value(g, &format!("{who}:g{}", i + 1))).collect();
        let next = self.grants.iter().position(|g| g.is_next).map_or(Value::Null, |i| format!("{who}:g{}", i + 1).into());
        obj([
            ("observed_at", self.observed_at.into()),
            ("source", self.source.as_str().into()),
            ("read_outcome", self.outcome.as_str().into()),
            ("eligible", opt(self.eligible)),
            ("ineligible_reason", text(&self.ineligible_reason)),
            ("next_grant_ref", next),
            ("grants", Value::Arr(grants)),
            ("five_hour_reset", Value::Null),
            ("cooldown_until", Value::Null),
            ("auto_use_enabled", Value::Null),
            ("last_reset", Value::Null),
        ])
    }

    /// A block as [`Resets::to_value`] writes it, or as devops writes it
    /// in its own feed; `None` when it is not one.  A block of source
    /// `not_read` says nobody looked, not that there are none: `None` too.
    pub fn from_value(v: &Value) -> Option<Resets> {
        if v.str_at("source") == Some("not_read") {
            return None;
        }
        let grants = v.get("grants")?;
        let next = v.str_at("next_grant_ref");
        Some(Resets {
            observed_at: time_at(v, "observed_at")?,
            source: v.str_at("source").unwrap_or("devops_feed").to_string(),
            outcome: v.str_at("read_outcome").and_then(Outcome::parse).unwrap_or(Outcome::Read),
            eligible: v.get("eligible").and_then(Value::bool),
            ineligible_reason: v.str_at("ineligible_reason").map(str::to_string),
            grants: grants.arr().iter().map(|g| grant_of(g, next)).collect(),
        })
    }
}

fn opt(b: Option<bool>) -> Value {
    b.map_or(Value::Null, Value::from)
}

fn text(s: &Option<String>) -> Value {
    s.as_deref().map_or(Value::Null, Value::from)
}

fn int(n: Option<i64>) -> Value {
    n.map_or(Value::Null, Value::from)
}

fn grant_value(g: &Grant, r#ref: &str) -> Value {
    obj([
        ("ref", r#ref.into()),
        ("kind", g.kind.as_str().into()),
        ("restores", Value::Arr(g.restores.iter().map(|w| w.as_str().into()).collect())),
        ("gives_back_weekly", g.gives_back_weekly.into()),
        ("moves_weekly_reset", g.moves_weekly_reset.into()),
        ("left", int(g.left.map(i64::from))),
        ("total", int(g.total.map(i64::from))),
        ("usable_now", opt(g.usable_now)),
        ("use_requires_limit", opt(g.use_requires_limit)),
        ("unavailable_reason", text(&g.unavailable_reason)),
        ("starts_at", int(g.starts_at)),
        ("ends_at", int(g.ends_at)),
        ("paid", false.into()),
        ("price_usd", Value::Null),
    ])
}

/// Unix seconds, written as a number or as an ISO-8601 UTC string.
fn time_at(v: &Value, key: &str) -> Option<i64> {
    match v.get(key)? {
        Value::Num(n) => Some(*n as i64),
        Value::Str(s) => crate::cc_usage::parse_iso_utc(s.clone()),
        _ => None,
    }
}

fn count_at(v: &Value, key: &str) -> Option<u32> {
    v.get(key).and_then(Value::i64).and_then(|n| u32::try_from(n).ok())
}

fn grant_of(g: &Value, next: Option<&str>) -> Grant {
    let restores = g.get("restores").map(Value::arr).unwrap_or_default();
    Grant {
        kind: g.str_at("kind").unwrap_or_default().to_string(),
        restores: restores.iter().filter_map(Value::str).map(str::to_string).collect(),
        gives_back_weekly: g.get("gives_back_weekly").and_then(Value::bool).unwrap_or(false),
        moves_weekly_reset: g.get("moves_weekly_reset").and_then(Value::bool).unwrap_or(false),
        left: count_at(g, "left"),
        total: count_at(g, "total"),
        usable_now: g.get("usable_now").and_then(Value::bool),
        use_requires_limit: g.get("use_requires_limit").and_then(Value::bool),
        unavailable_reason: g.str_at("unavailable_reason").map(str::to_string),
        starts_at: time_at(g, "starts_at"),
        ends_at: time_at(g, "ends_at"),
        is_next: next.is_some_and(|n| g.str_at("ref") == Some(n)),
    }
}

/// One account's row of the file.
#[derive(Clone, Debug, PartialEq)]
pub struct AccountResets {
    /// `claude`, `codex`, `kimi`.
    pub agent: String,
    /// The account's number; 0 for the unnumbered home.
    pub profile: u8,
    /// As the usage feed names it (`Codex 1`).
    pub name: String,
    pub resets: Resets,
}

/// The file's text.  Grants that ended by `now` are left out.
pub fn render(accounts: &[AccountResets], now: i64) -> String {
    let rows = accounts
        .iter()
        .map(|a| {
            let who = format!("{}:{}", a.agent, a.profile);
            obj([
                ("agent", a.agent.as_str().into()),
                ("profile", i64::from(a.profile).into()),
                ("name", a.name.as_str().into()),
                ("resets", a.resets.clone().live(now).to_value(&who)),
            ])
        })
        .collect();
    obj([("generated_at", now.into()), ("accounts", Value::Arr(rows))]).to_json()
}

/// The accounts of a file [`render`] wrote; nothing for anything else.
pub fn parse(body: &str) -> Vec<AccountResets> {
    let Ok(v) = json::parse(body) else { return Vec::new() };
    let rows = v.get("accounts").map(Value::arr).unwrap_or_default();
    rows.iter()
        .filter_map(|r| {
            Some(AccountResets {
                agent: r.str_at("agent")?.to_string(),
                profile: u8::try_from(r.get("profile")?.i64()?).ok()?,
                name: r.str_at("name").unwrap_or_default().to_string(),
                resets: Resets::from_value(r.get("resets")?)?,
            })
        })
        .collect()
}

pub fn path() -> PathBuf {
    marspot_term::paths::state_root().join("usage/resets.json")
}

/// What the collector last wrote, as of `now`: no grant that has ended.
pub fn read(now: i64) -> Vec<AccountResets> {
    let accounts = parse(&std::fs::read_to_string(path()).unwrap_or_default());
    accounts.into_iter().map(|a| AccountResets { resets: a.resets.live(now), ..a }).collect()
}

#[cfg(test)]
#[path = "agent_resets_tests.rs"]
mod tests;
