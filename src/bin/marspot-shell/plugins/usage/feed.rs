//! What is known about each account, and the feed file it is written
//! to: the schema `marspot::cc_usage` reads.
//!
//! An account keeps the last observation of each of its windows, with
//! when it was made.  A pass that could not read an account leaves its
//! numbers as they were, so one refused request does not blank a row.

use std::path::Path;

use marspot_peer::json::{self, obj, Value};

/// A window nobody has read for this long is dropped.
const KEEP_SECS: i64 = 7 * 86_400;
/// A window whose reset has passed is dropped once it is this old: the
/// number described the period that ended.
const PAST_RESET_KEEP_SECS: i64 = 15 * 60;

const FIVE_HOUR_MINUTES: i64 = 300;
pub(super) const WEEK_MINUTES: i64 = 10_080;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Window {
    /// 0.0 ..= 1.0
    pub util: f64,
    /// Unix seconds.
    pub reset: Option<i64>,
    pub severity: Option<String>,
    pub is_active: Option<bool>,
    /// Unix seconds this was observed.
    pub seen: i64,
}

impl Window {
    pub(super) fn new(util: f64, reset: Option<i64>, seen: i64) -> Window {
        Window { util, reset, severity: None, is_active: None, seen }
    }

    fn expired(&self, now: i64) -> bool {
        let age = now - self.seen;
        age > KEEP_SECS || (age > PAST_RESET_KEEP_SECS && self.reset.is_some_and(|r| r <= now))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ModelWindow {
    pub label: String,
    pub minutes: i64,
    pub window: Window,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Credits {
    pub enabled: Option<bool>,
    pub unlimited: Option<bool>,
    pub balance: Option<f64>,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub utilization: Option<f64>,
    pub disabled_reason: Option<String>,
}

/// What one successful read of an account returned.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Reading {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    pub models: Vec<ModelWindow>,
    pub credits: Option<Credits>,
    /// The provider itself says the account is refused, whatever the
    /// windows read.
    pub limit_reached: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Account {
    pub name: String,
    pub email: String,
    pub tier: String,
    pub auth_error: bool,
    pub limit_reached: bool,
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    pub models: Vec<ModelWindow>,
    pub credits: Option<Credits>,
    /// Unix seconds of the last successful read; 0 for none yet.
    pub last_read: i64,
}

impl Account {
    /// Take a successful read: each window it carries replaces the one
    /// held, and a window it does not carry stays until it expires.
    pub(super) fn absorb(&mut self, r: Reading, now: i64) {
        self.auth_error = false;
        self.limit_reached = r.limit_reached;
        self.last_read = now;
        if r.five_hour.is_some() {
            self.five_hour = r.five_hour;
        }
        if r.seven_day.is_some() {
            self.seven_day = r.seven_day;
        }
        for m in r.models {
            match self.models.iter_mut().find(|h| h.label == m.label && h.minutes == m.minutes) {
                Some(held) => *held = m,
                None => self.models.push(m),
            }
        }
        if r.credits.is_some() {
            self.credits = r.credits;
        }
    }

    pub(super) fn expire(&mut self, now: i64) {
        if self.five_hour.as_ref().is_some_and(|w| w.expired(now)) {
            self.five_hour = None;
        }
        if self.seven_day.as_ref().is_some_and(|w| w.expired(now)) {
            self.seven_day = None;
        }
        self.models.retain(|m| !m.window.expired(now));
    }

    pub(super) fn has_reading(&self) -> bool {
        self.last_read > 0
    }

    pub(super) fn status(&self) -> &'static str {
        let full = |w: &Window| w.util >= 1.0;
        if self.auth_error {
            "auth_error"
        } else if self.limit_reached
            || self.five_hour.as_ref().is_some_and(full)
            || self.seven_day.as_ref().is_some_and(full)
            || self.models.iter().any(|m| full(&m.window))
        {
            "rejected"
        } else {
            "allowed"
        }
    }

    fn collected_at(&self) -> i64 {
        let seen = |w: &Option<Window>| w.as_ref().map(|w| w.seen);
        seen(&self.seven_day).or(seen(&self.five_hour)).unwrap_or(self.last_read)
    }

    /// When the model windows were read, if that is not when the row
    /// was: the reader then knows those numbers are older than the row.
    fn limits_as_of(&self) -> Option<i64> {
        let oldest = self.models.iter().map(|m| m.window.seen).min()?;
        (oldest != self.collected_at()).then_some(oldest)
    }

    fn to_value(&self) -> Value {
        let util = |w: &Option<Window>| num(w.as_ref().map(|w| w.util));
        let reset = |w: &Option<Window>| int(w.as_ref().and_then(|w| w.reset));
        let models = self.models.iter().map(model_value).collect();
        let mut row = obj([
            ("name", self.name.as_str().into()),
            ("email", self.email.as_str().into()),
            ("status", self.status().into()),
            ("utilization_5h", util(&self.five_hour)),
            ("utilization_7d", util(&self.seven_day)),
            ("reset_5h", reset(&self.five_hour)),
            ("reset_7d", reset(&self.seven_day)),
            ("model_limits", Value::Arr(models)),
            ("credits", self.credits.as_ref().map_or(Value::Null, credits_value)),
            ("tier", self.tier.as_str().into()),
        ]);
        if let Some(at) = self.limits_as_of() {
            row.set("limits_as_of", iso_utc(at).into());
        }
        row.set("collected_at", iso_utc(self.collected_at()).into());
        row
    }
}

fn num(n: Option<f64>) -> Value {
    n.map_or(Value::Null, Value::Num)
}

fn int(n: Option<i64>) -> Value {
    n.map_or(Value::Null, Value::from)
}

fn text(s: &Option<String>) -> Value {
    s.as_deref().map_or(Value::Null, Value::from)
}

fn flag(b: Option<bool>) -> Value {
    b.map_or(Value::Null, Value::from)
}

fn model_value(m: &ModelWindow) -> Value {
    obj([
        ("label", m.label.as_str().into()),
        ("utilization", Value::Num(m.window.util)),
        ("reset", int(m.window.reset)),
        ("window_minutes", m.minutes.into()),
        ("severity", text(&m.window.severity)),
        ("is_active", flag(m.window.is_active)),
    ])
}

fn credits_value(c: &Credits) -> Value {
    obj([
        ("enabled", flag(c.enabled)),
        ("unlimited", flag(c.unlimited)),
        ("balance", num(c.balance)),
        ("used", num(c.used)),
        ("limit", num(c.limit)),
        ("utilization", num(c.utilization)),
        ("disabled_reason", text(&c.disabled_reason)),
        ("resets_available", Value::Null),
        ("resets_expire_at", Value::Null),
    ])
}

/// The feed file's text.  An account never read and not refused has
/// nothing to say and gets no row: a row of blanks would read as an
/// account with all of its allowance left.
pub(super) fn render(accounts: &[Account], now: i64) -> String {
    let rows = accounts
        .iter()
        .filter(|a| a.has_reading() || a.auth_error)
        .map(Account::to_value)
        .collect();
    obj([("generated_at", iso_utc(now).into()), ("accounts", Value::Arr(rows))]).to_json()
}

/// The accounts in a feed file this module wrote, so a restart carries
/// on from the last observations instead of from nothing.
pub(super) fn load(body: &str) -> Vec<Account> {
    let Ok(v) = json::parse(body) else {
        return Vec::new();
    };
    v.get("accounts").map(Value::arr).unwrap_or_default().iter().map(account_of).collect()
}

fn f64_at(v: &Value, key: &str) -> Option<f64> {
    match v.get(key) {
        Some(Value::Num(n)) => Some(*n),
        _ => None,
    }
}

fn iso_at(v: &Value, key: &str) -> Option<i64> {
    marspot::cc_usage::parse_iso_utc(v.str_at(key)?.to_string())
}

fn account_of(row: &Value) -> Account {
    let collected = iso_at(row, "collected_at").unwrap_or(0);
    let limits_seen = iso_at(row, "limits_as_of").unwrap_or(collected);
    let own = |util: &str, reset: &str| {
        let reset = row.get(reset).and_then(Value::i64);
        f64_at(row, util).map(|u| Window::new(u, reset, collected))
    };
    let models = row.get("model_limits").map(Value::arr).unwrap_or_default();
    let mut a = Account {
        name: row.str_at("name").unwrap_or_default().to_string(),
        email: row.str_at("email").unwrap_or_default().to_string(),
        tier: row.str_at("tier").unwrap_or_default().to_string(),
        auth_error: row.str_at("status") == Some("auth_error"),
        limit_reached: false,
        five_hour: own("utilization_5h", "reset_5h"),
        seven_day: own("utilization_7d", "reset_7d"),
        models: models.iter().filter_map(|m| model_of(m, limits_seen)).collect(),
        credits: row.get("credits").filter(|c| **c != Value::Null).map(credits_of),
        last_read: collected,
    };
    // a refusal the windows do not account for was the provider's word
    a.limit_reached = row.str_at("status") == Some("rejected") && a.status() != "rejected";
    a
}

fn model_of(m: &Value, seen: i64) -> Option<ModelWindow> {
    Some(ModelWindow {
        label: m.str_at("label")?.to_string(),
        minutes: m.get("window_minutes").and_then(Value::i64).unwrap_or(WEEK_MINUTES),
        window: Window {
            util: f64_at(m, "utilization")?,
            reset: m.get("reset").and_then(Value::i64),
            severity: m.str_at("severity").map(str::to_string),
            is_active: m.get("is_active").and_then(Value::bool),
            seen,
        },
    })
}

fn credits_of(c: &Value) -> Credits {
    Credits {
        enabled: c.get("enabled").and_then(Value::bool),
        unlimited: c.get("unlimited").and_then(Value::bool),
        balance: f64_at(c, "balance"),
        used: f64_at(c, "used"),
        limit: f64_at(c, "limit"),
        utilization: f64_at(c, "utilization"),
        disabled_reason: c.str_at("disabled_reason").map(str::to_string),
    }
}

/// Which of an account's own windows a span of `minutes` is.
pub(super) fn own_window(minutes: i64) -> Option<OwnWindow> {
    match minutes {
        FIVE_HOUR_MINUTES => Some(OwnWindow::FiveHour),
        WEEK_MINUTES => Some(OwnWindow::SevenDay),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OwnWindow {
    FiveHour,
    SevenDay,
}

/// Replace `path` in one step, so a reader never sees half a file.
pub(super) fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// Unix seconds as `2026-11-10T05:55:14+00:00`, the form the feed's
/// reader and the devops collector both use.
pub(super) fn iso_utc(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    // civil-from-days, Howard Hinnant's algorithm
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}+00:00",
        secs / 3_600,
        secs % 3_600 / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_290_114; // 2026-11-10T05:55:14Z

    fn sample() -> Account {
        let mut fable = Window::new(0.91, Some(1_794_538_799), NOW - 6);
        fable.severity = Some("critical".into());
        fable.is_active = Some(false);
        Account {
            name: "Claude 1".into(),
            email: "a@b".into(),
            tier: "default_claude_max_20x".into(),
            seven_day: Some(Window::new(0.42, Some(1_794_538_799), NOW - 6)),
            models: vec![ModelWindow { label: "Fable".into(), minutes: WEEK_MINUTES, window: fable }],
            credits: Some(Credits {
                enabled: Some(false),
                used: Some(0.0),
                utilization: Some(0.0),
                disabled_reason: Some("user_disabled".into()),
                ..Credits::default()
            }),
            last_read: NOW - 6,
            ..Account::default()
        }
    }

    #[test]
    fn times_are_written_the_way_the_reader_parses_them() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00+00:00");
        assert_eq!(iso_utc(NOW), "2026-11-10T05:55:14+00:00");
        assert_eq!(iso_utc(1_709_164_800), "2024-02-29T00:00:00+00:00");
        for t in [0, NOW, 1_709_164_800, 1_784_423_639] {
            assert_eq!(marspot::cc_usage::parse_iso_utc(iso_utc(t)), Some(t));
        }
    }

    #[test]
    fn the_written_feed_has_the_schema_keys_in_order() {
        let body = render(&[sample()], NOW);
        assert_eq!(
            body,
            concat!(
                r#"{"generated_at":"2026-11-10T05:55:14+00:00","accounts":[{"name":"Claude 1","email":"a@b","#,
                r#""status":"allowed","utilization_5h":null,"utilization_7d":0.42,"reset_5h":null,"#,
                r#""reset_7d":1794538799,"model_limits":[{"label":"Fable","utilization":0.91,"#,
                r#""reset":1794538799,"window_minutes":10080,"severity":"critical","is_active":false}],"#,
                r#""credits":{"enabled":false,"unlimited":null,"balance":null,"used":0,"limit":null,"#,
                r#""utilization":0,"disabled_reason":"user_disabled","resets_available":null,"#,
                r#""resets_expire_at":null},"tier":"default_claude_max_20x","#,
                r#""collected_at":"2026-11-10T05:55:08+00:00"}]}"#
            )
        );
    }

    #[test]
    fn the_written_feed_reads_back_through_the_readers_parser() {
        let mut second = sample();
        second.name = "Claude 2".into();
        second.email = "c@d".into();
        second.tier = "default_claude_max_5x".into();
        second.five_hour = Some(Window::new(1.0, Some(NOW + 600), NOW - 6));
        second.models[0].window.seen = NOW - 3_600;
        let u = marspot::cc_usage::parse(&render(&[sample(), second], NOW)).expect("parses");
        assert_eq!(u.generated_at, NOW);
        let a = &u.accounts[0];
        assert_eq!((a.name.as_str(), a.email.as_str(), a.status.as_str()), ("Claude 1", "a@b", "allowed"));
        assert_eq!((a.util_5h, a.reset_5h), (None, None), "a missing 5h window is null, which reads as none");
        assert_eq!((a.util_7d, a.reset_7d), (0.42, 1_794_538_799));
        assert_eq!(a.model_limits.len(), 1);
        assert_eq!(a.model_limits[0].label, "Fable");
        assert_eq!(a.model_limits[0].util, 0.91);
        assert_eq!(a.model_limits[0].reset, Some(1_794_538_799));
        assert_eq!(a.tier, "default_claude_max_20x");
        assert_eq!(a.collected_at, NOW - 6);
        assert_eq!(a.limits_as_of, None, "the model window was read with the row");

        let b = &u.accounts[1];
        assert_eq!(b.status, "rejected", "a full window refuses the account");
        assert_eq!((b.util_5h, b.reset_5h), (Some(1.0), Some(NOW + 600)));
        assert_eq!(b.limits_as_of, Some(NOW - 3_600), "an older model window says how old");
    }

    #[test]
    fn a_codex_row_reads_back_through_the_codex_parser() {
        let a = Account {
            name: "Codex 1".into(),
            email: "a@b".into(),
            tier: "pro".into(),
            limit_reached: true,
            seven_day: Some(Window::new(0.5, Some(1_794_427_376), NOW)),
            last_read: NOW,
            ..Account::default()
        };
        let u = marspot::cc_usage::parse_codex(&render(&[a], NOW)).expect("parses");
        let row = &u.accounts[0];
        assert_eq!((row.name.as_str(), row.status.as_str()), ("Codex 1", "rejected"));
        assert_eq!(row.windows.len(), 1);
        assert_eq!((row.windows[0].util, row.windows[0].reset), (0.5, Some(1_794_427_376)));
    }

    #[test]
    fn a_restart_reloads_the_last_observations_from_the_feed() {
        let mut stale_models = sample();
        stale_models.models[0].window.seen = NOW - 3_600;
        let mut refused = sample();
        refused.email = "c@d".into();
        refused.auth_error = true;
        let mut codex = sample();
        codex.email = "e@f".into();
        codex.limit_reached = true;
        let held = vec![sample(), stale_models, refused, codex];
        assert_eq!(load(&render(&held, NOW)), held);
        assert!(load("not json").is_empty());
    }

    #[test]
    fn an_account_never_read_has_no_row_unless_it_was_refused() {
        let unread = Account { name: "Claude 3".into(), email: "x@y".into(), ..Account::default() };
        assert_eq!(load(&render(std::slice::from_ref(&unread), NOW)), Vec::new());
        let refused = Account { auth_error: true, ..unread };
        let u = marspot::cc_usage::parse(&render(&[refused], NOW)).expect("one row");
        assert_eq!(u.accounts[0].status, "auth_error");
    }

    #[test]
    fn a_failed_pass_keeps_the_numbers_and_a_good_one_replaces_them() {
        let mut a = sample();
        let before = a.clone();
        a.expire(NOW + 300);
        assert_eq!(a, before, "five minutes on, nothing has been read and nothing is dropped");

        let later = NOW + 300;
        a.absorb(
            Reading { seven_day: Some(Window::new(0.5, Some(1_794_538_799), later)), ..Reading::default() },
            later,
        );
        assert_eq!(a.seven_day.as_ref().map(|w| (w.util, w.seen)), Some((0.5, later)));
        assert_eq!(a.models, before.models, "a window the read did not carry stays");
        assert_eq!(a.credits, before.credits);
        assert_eq!(a.limits_as_of(), Some(NOW - 6), "and is dated when it was read");
    }

    #[test]
    fn a_window_is_dropped_after_a_week_or_once_its_reset_is_behind_it() {
        let reset = NOW + 3_600;
        let w = Window::new(0.3, Some(reset), NOW);
        assert!(!w.expired(reset - 1), "its period is still running");
        assert!(w.expired(reset), "the period it described has ended");
        let fresh = Window::new(0.3, Some(NOW - 60), NOW);
        assert!(!fresh.expired(NOW + PAST_RESET_KEEP_SECS), "just read, whatever its reset says");
        assert!(fresh.expired(NOW + PAST_RESET_KEEP_SECS + 1));
        let open = Window::new(0.3, None, NOW);
        assert!(!open.expired(NOW + KEEP_SECS), "no reset: kept for the week");
        assert!(open.expired(NOW + KEEP_SECS + 1));

        let mut a = sample();
        a.five_hour = Some(Window::new(0.2, Some(NOW + 60), NOW - 6));
        a.expire(NOW + 3_600);
        assert_eq!(a.five_hour, None);
        assert!(a.seven_day.is_some() && a.models.len() == 1, "their reset is days away");
    }

    #[test]
    fn the_status_is_auth_error_then_rejected_then_allowed() {
        let mut a = sample();
        assert_eq!(a.status(), "allowed");
        a.models[0].window.util = 1.0;
        assert_eq!(a.status(), "rejected");
        a.auth_error = true;
        assert_eq!(a.status(), "auth_error");
        let provider_says = Account { limit_reached: true, ..sample() };
        assert_eq!(provider_says.status(), "rejected");
    }

    #[test]
    fn the_feed_is_replaced_whole() {
        let dir = marspot_term::test_env::TempDir::new("usage-feed-write");
        let path = dir.join("usage/claude-usage.json");
        write_atomic(&path, "one").unwrap();
        write_atomic(&path, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        let left: Vec<_> = std::fs::read_dir(dir.join("usage")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, ["claude-usage.json"], "no temporary file is left behind");
    }
}
