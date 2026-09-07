//! cc — agent usage feeds for the toolbar `Cc` modal.
//!
//! The devops side drops a small JSON snapshot per provider —
//! `~/.local/state/devops/claude-usage.json` and `codex-usage.json`
//! (one object per account: rolling window utilization + the unix
//! reset instants).  This module reads + parses them with a
//! purpose-built scanner — the schema is fixed and tiny, a JSON
//! crate would be a dependency for one file (self-build principle).
//!
//! The two feeds share a schema but not a shape: Anthropic meters one
//! account-wide 5h and 7d window, OpenAI meters no account-wide 5h at
//! all and puts every allowance on a model.  Both are reduced here to
//! the same thing — a list of named windows — so the panel has one way
//! to draw a bar rather than one per provider.
//!
//! Everything here is read-only and cold-path: the file is touched
//! only while the modal is open (open + 5 s refresh), never on the
//! render hot path.

use std::path::PathBuf;

/// A per-model cap the feed reports alongside the rolling windows.
///
/// The account-level 5h/7d numbers do not cover these: an account can
/// be at 7 % of its week and still be shut out of one model, which is
/// the single most useful thing to know before starting a session.
#[derive(Debug, Clone, PartialEq)]
pub struct CcModelLimit {
    /// Model name as the feed writes it ("Fable").
    pub label: String,
    /// 0.0 ..= 1.0 utilization of that model's own window.
    pub util: f64,
    /// Unix seconds when it resets; `None` when nothing has been used
    /// and the window has not started.
    pub reset: Option<i64>,
}

/// One Claude account row from the feed.
#[derive(Debug, Clone, PartialEq)]
pub struct CcAccount {
    pub name: String,
    pub email: String,
    /// Raw status string from the feed ("allowed", …).  The modal
    /// renders "ok" for allowed and the raw string otherwise.
    pub status: String,
    /// 0.0 ..= 1.0 rolling-window utilization.
    pub util_5h: f64,
    pub util_7d: f64,
    /// Unix seconds when each window resets.
    pub reset_5h: i64,
    pub reset_7d: i64,
    /// Per-model caps, in feed order.  Empty on an older feed.
    pub model_limits: Vec<CcModelLimit>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CcUsage {
    /// Unix seconds the snapshot was generated (parsed from the
    /// feed's ISO-8601 `generated_at`, which the generator emits in
    /// UTC).
    pub generated_at: i64,
    pub accounts: Vec<CcAccount>,
}

/// How the modal should present an account's rolling-window status.
/// The feed carries the raw `anthropic-ratelimit-unified-status`
/// response header verbatim; that header is not covered by the public
/// API docs (it's the subscription-side unified limiter, not the
/// per-org API rate limiter), so this maps only the values the
/// collector has actually observed and lets anything else through
/// as raw text rather than guessing at a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcStatusKind {
    /// `allowed` — within limits.
    Ok,
    /// `allowed_warning` — still served, but the window is close
    /// enough to its cap that the header flags it.
    Warn,
    /// `rejected` — the window is exhausted; requests are refused.
    Limited,
    /// Collector-side sentinel: no token for this account.
    NoToken,
    /// Anything else (including `unknown`) — shown as raw text.
    Other,
}

impl CcStatusKind {
    pub fn classify(raw: &str) -> Self {
        match raw {
            "allowed" => Self::Ok,
            "allowed_warning" => Self::Warn,
            "rejected" => Self::Limited,
            "no_token" => Self::NoToken,
            _ => Self::Other,
        }
    }

    /// Short label for the card corner.  Kept to ≤ 7 chars so the
    /// layout can reserve a fixed slot and never collide with the
    /// email to its left.
    pub fn label(self, raw: &str) -> String {
        match self {
            Self::Ok => "ok".into(),
            Self::Warn => "near".into(),
            Self::Limited => "limit".into(),
            Self::NoToken => "no key".into(),
            // Unknown status: surface the source string, truncated to
            // the reserved slot so a future value can't break layout.
            Self::Other => raw.chars().take(7).collect(),
        }
    }
}

/// Feed location.  `$HOME/.local/state/devops/claude-usage.json`.
pub fn feed_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local/state/devops/claude-usage.json")
}

/// Read + parse the feed.  `None` on missing file / parse failure —
/// the modal shows a "no usage feed" placeholder instead of erroring.
pub fn read() -> Option<CcUsage> {
    let body = std::fs::read_to_string(feed_path()).ok()?;
    parse(&body)
}

/// Parse the fixed-schema feed.  Tolerant of field order and
/// whitespace; ignores unknown fields.  Not a general JSON parser —
/// strings in this feed never contain escaped quotes, and the
/// generator writes numbers plainly.
pub fn parse(body: &str) -> Option<CcUsage> {
    let generated_at = str_field(body, "generated_at")
        .and_then(parse_iso_utc)
        .unwrap_or(0);
    let accounts_start = body.find("\"accounts\"")?;
    let arr_start = body[accounts_start..].find('[')? + accounts_start;
    let accounts: Vec<CcAccount> = objects_in_array(&body[arr_start..])
        .into_iter()
        .map(|obj| CcAccount {
            name: str_field(obj, "name").unwrap_or_default(),
            email: str_field(obj, "email").unwrap_or_default(),
            status: str_field(obj, "status").unwrap_or_default(),
            util_5h: num_field(obj, "utilization_5h").unwrap_or(0.0),
            util_7d: num_field(obj, "utilization_7d").unwrap_or(0.0),
            reset_5h: num_field(obj, "reset_5h").unwrap_or(0.0) as i64,
            reset_7d: num_field(obj, "reset_7d").unwrap_or(0.0) as i64,
            model_limits: model_limits(obj),
        })
        .collect();
    let mut accounts = accounts;
    if accounts.is_empty() {
        return None;
    }
    // The collector writes accounts in completion order, so the feed
    // arrives shuffled (3, 1, 2, 4) and changes between refreshes.  Sort
    // here rather than in the painter: the modal draws the same list
    // twice — cards and timeline rows — and those two must agree.
    accounts.sort_by(|a, b| natural_cmp(&a.name, &b.name).then_with(|| a.email.cmp(&b.email)));
    Some(CcUsage { generated_at, accounts })
}

/// Compare names the way a reader scans them: digit runs compare as
/// numbers, everything else bytewise.
///
/// Plain string ordering is wrong here — these names end in an index,
/// and `"Claude 10" < "Claude 2"` lexicographically.  Four accounts
/// don't expose that today; a fifth-through-tenth would.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let (si, sj) = (i, j);
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            // Leading zeros carry no magnitude, so strip them before
            // comparing by width.
            let da = strip_zeros(&a[si..i]);
            let db = strip_zeros(&b[sj..j]);
            match da.len().cmp(&db.len()).then_with(|| da.cmp(db)) {
                Ordering::Equal => {}
                other => return other,
            }
        } else {
            match a[i].cmp(&b[j]) {
                Ordering::Equal => {
                    i += 1;
                    j += 1;
                }
                other => return other,
            }
        }
    }
    (a.len() - i).cmp(&(b.len() - j))
}

fn strip_zeros(digits: &[u8]) -> &[u8] {
    let start = digits.iter().position(|&d| d != b'0').unwrap_or(digits.len());
    &digits[start..]
}

/// The `model_limits` array of one account object, if it has one.
fn model_limits(obj: &str) -> Vec<CcModelLimit> {
    let Some(arr) = top_level_value(obj, "model_limits") else {
        return Vec::new();
    };
    objects_in_array(arr)
        .into_iter()
        .map(|m| CcModelLimit {
            label: str_field(m, "label").unwrap_or_default(),
            util: num_field(m, "utilization").unwrap_or(0.0),
            // `"reset": null` is not a time; a model nobody has touched
            // has no window to reset.  `num_field` fails on `null`,
            // which is the answer.
            reset: num_field(m, "reset").map(|t| t as i64),
        })
        .collect()
}

/// Every `{...}` directly inside the array `s` starts with, with
/// nesting respected.
///
/// The first version took "the next `}` closes the object", which held
/// while accounts were flat.  The feed since grew `model_limits` and
/// `credits` sub-objects, so that `}` became the end of an inner
/// object: account 1 parsed from a truncated slice (its scalar fields
/// happen to precede the nesting, so it looked fine) and the scan then
/// hit the `]` closing `model_limits` and called that the end of the
/// array.  Three of four accounts vanished from the panel with no
/// error anywhere — the failure a hand-rolled scanner has to be
/// written against.
fn objects_in_array(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'"' => i = skip_string(b, i),
            b'{' | b'[' => {
                depth += 1;
                // depth 1 is the array itself, so an object opening at
                // depth 2 is one of its elements.
                if depth == 2 && b[i] == b'{' {
                    start = i;
                }
                i += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                if depth == 1 && b[i] == b'}' {
                    out.push(&s[start..=i]);
                } else if depth == 0 {
                    return out;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// Index just past the string literal starting at `b[i] == '"'`.
fn skip_string(b: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return j + 1,
            _ => j += 1,
        }
    }
    j
}

/// The value of `key` **at this object's own level**, as a slice
/// starting at its first byte.
///
/// Depth-aware for the same reason as [`objects_in_array`]: with
/// sub-objects in play, a plain `find("\"status\"")` would happily
/// match a nested field, and which one it hits would depend on the
/// order the generator happens to write.
fn top_level_value<'a>(obj: &'a str, key: &str) -> Option<&'a str> {
    let b = obj.as_bytes();
    let mut depth = 0i32;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let end = skip_string(b, i);
                // A string is a key only if a colon follows it; without
                // that test the *value* `"name": "status"` would answer
                // a lookup for `status`.
                let after = obj[end..].trim_start();
                if depth == 1 && after.starts_with(':') && &obj[i + 1..end - 1] == key {
                    return Some(after[1..].trim_start());
                }
                i = end;
            }
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// `"key": "value"` string extractor.
fn str_field(obj: &str, key: &str) -> Option<String> {
    let v = top_level_value(obj, key)?.strip_prefix('"')?;
    let end = v.find('"')?;
    Some(v[..end].to_string())
}

/// `"key": 12.34` number extractor.
fn num_field(obj: &str, key: &str) -> Option<f64> {
    let tail = top_level_value(obj, key)?;
    let end = tail
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E'))
        .unwrap_or(tail.len());
    tail[..end].parse().ok()
}

/// `2026-07-19T01:13:59.715798+00:00` → unix seconds.  The feed's
/// generator always emits UTC; fractional seconds and the offset
/// suffix are ignored (offset is `+00:00` by construction).
fn parse_iso_utc(s: String) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        std::str::from_utf8(&b[r]).ok()?.parse().ok()
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Days since epoch — civil-from-days inverse (Howard Hinnant's
    // algorithm, integer-only).
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3_600 + mi * 60 + sec)
}

/// Unix seconds → local wall-clock `(month, day, hour, minute)` via
/// `localtime_r` — the modal renders reset instants in the user's
/// timezone, matching every other clock they look at.
pub fn local_mdhm(unix: i64) -> (u32, u32, u32, u32) {
    let t = unix as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    (
        (tm.tm_mon + 1) as u32,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
    )
}

/// The local midnight at or before `unix`.
///
/// The day grid in the timeline is *labelled* with local dates, so it
/// has to be *placed* on local days.  It used to step in flat 86 400 s
/// from a UTC-aligned start, which put every rule the timezone's
/// offset away from the date written under it — nine hours, in JST.
/// A 7-day window resetting at 00:00 on the 8th therefore ended
/// visibly to the LEFT of the rule labelled `8/8`, and the bar and its
/// own label disagreed with the axis (2026-08-07 report, long-standing).
pub fn local_day_start(unix: i64) -> i64 {
    let t = unix as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    tm.tm_hour = 0;
    tm.tm_min = 0;
    tm.tm_sec = 0;
    // Let libc decide DST for that wall-clock instant rather than
    // carrying over the flag from the time we started from.
    tm.tm_isdst = -1;
    unsafe { libc::mktime(&mut tm) as i64 }
}

/// The local midnight strictly after `unix`.
///
/// One *calendar* day on, which is 23 or 25 hours across a DST
/// boundary — never assume 86 400.  `mktime` normalises the overflowed
/// `tm_mday` for us, so month and year ends need no special case.
pub fn next_local_day_start(unix: i64) -> i64 {
    let t = unix as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    tm.tm_mday += 1;
    tm.tm_hour = 0;
    tm.tm_min = 0;
    tm.tm_sec = 0;
    tm.tm_isdst = -1;
    unsafe { libc::mktime(&mut tm) as i64 }
}

/// Widen `[t0, t1]` out to the local midnights that bracket it.
///
/// The plot's margin and its readability rule in one: every bar then
/// has a dated rule on both sides of it.  Without this the range ended
/// wherever the data did, so the last bar ran past the final rule with
/// nothing behind it to read against — `7d 19% 12:00` sitting to the
/// right of `8/14`, and no `8/15` drawn (2026-08-07 report).
pub fn snap_range_to_local_days(t0: f64, t1: f64) -> (f64, f64) {
    let lo = local_day_start(t0 as i64);
    let hi = {
        let s = local_day_start(t1 as i64);
        if (s as f64) < t1 { next_local_day_start(s) } else { s }
    };
    (lo as f64, hi as f64)
}

/// One metered window, reduced to what a card row and a timeline bar
/// need.  Both providers land here.
///
/// The panel used to hard-code "an account has a 5h and a 7d, plus
/// some model caps".  That is Anthropic's shape, not a general one:
/// OpenAI meters no account-wide 5h at all — its only sub-day window
/// belongs to one model — so a Codex account rendered through those
/// fields would have had to lie about which window it was drawing.
#[derive(Debug, Clone, PartialEq)]
pub struct CcWindow {
    /// Row label, already in the panel's all-caps style.
    pub label: String,
    /// 0.0 ..= 1.0 utilization of this window.
    pub util: f64,
    /// Unix seconds when it resets. `None` = nothing used yet / not
    /// reported; the card still draws the row, the timeline draws no
    /// bar (there is no extent to draw).
    pub reset: Option<i64>,
    /// How long the window is. The bar runs `[reset - span, reset]`.
    pub span_secs: i64,
}

const WEEK_SECS: i64 = 7 * 86_400;

impl CcAccount {
    /// The account's windows in reading order: its own two, then one
    /// per model cap.  A model cap is weekly on this provider.
    pub fn windows(&self) -> Vec<CcWindow> {
        let mut out = vec![
            CcWindow {
                label: "5H".into(),
                util: self.util_5h,
                reset: (self.reset_5h > 0).then_some(self.reset_5h),
                span_secs: 5 * 3_600,
            },
            CcWindow {
                label: "7D".into(),
                util: self.util_7d,
                reset: (self.reset_7d > 0).then_some(self.reset_7d),
                span_secs: WEEK_SECS,
            },
        ];
        out.extend(self.model_limits.iter().map(|m| CcWindow {
            label: m.label.to_uppercase(),
            util: m.util,
            reset: m.reset,
            span_secs: WEEK_SECS,
        }));
        out
    }
}

/// One Codex account, already reduced to its windows.
///
/// Unlike [`CcAccount`] the raw fields are not kept: on this provider
/// the account-level 5h is always absent and the account-level 7d is
/// one row among four, so there is nothing a caller could do with them
/// that `windows` does not already say.
#[derive(Debug, Clone, PartialEq)]
pub struct CxAccount {
    pub name: String,
    pub email: String,
    pub status: String,
    pub windows: Vec<CcWindow>,
}

/// Codex feed location. `$HOME/.local/state/devops/codex-usage.json`.
pub fn codex_feed_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local/state/devops/codex-usage.json")
}

/// Read + parse the Codex feed. `None` on missing file / parse failure.
pub fn read_codex() -> Option<CodexUsage> {
    let body = std::fs::read_to_string(codex_feed_path()).ok()?;
    parse_codex(&body)
}

/// The Codex feed, parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexUsage {
    pub generated_at: i64,
    pub accounts: Vec<CxAccount>,
}

/// A bucket's model codename, without the window span.
///
/// `GPT-5.3-Codex-Spark` -> `SPARK`, `gpt-reserve` -> `RESERVE`.  A
/// bucket metering two windows carries the span after a space
/// (`GPT-5.3-Codex-Spark 7d`) — the span is not part of the model's
/// name, and leaving it in produces a row nothing can be looked up by.
fn codex_model_name(label: &str) -> String {
    let name = label.split_whitespace().next().unwrap_or(label);
    name.rsplit('-').next().unwrap_or(name).to_uppercase()
}

/// The four rows of one Codex account, in reading order:
///
///   5H       the only sub-day allowance Codex meters, and it
///            constrains Spark alone
///   SPARK    Spark's weekly
///   ASTRA    the account-wide weekly — what the default model draws on
///   RESERVE  gpt-reserve's own weekly
///
/// The point of per-model metering is knowing where there is headroom,
/// so every bucket gets a row; showing only the busiest hides the ones
/// worth switching to.  A bucket nobody named here still gets a row
/// rather than disappearing.
fn codex_windows(obj: &str) -> Vec<CcWindow> {
    struct Bucket {
        name: String,
        util: f64,
        reset: Option<i64>,
        span: i64,
    }
    let buckets: Vec<Bucket> = match top_level_value(obj, "model_limits") {
        None => Vec::new(),
        Some(arr) => objects_in_array(arr)
            .into_iter()
            .map(|m| {
                let minutes = num_field(m, "window_minutes").unwrap_or(0.0) as i64;
                Bucket {
                    name: codex_model_name(&str_field(m, "label").unwrap_or_default()),
                    util: num_field(m, "utilization").unwrap_or(0.0),
                    reset: num_field(m, "reset").map(|t| t as i64),
                    span: minutes * 60,
                }
            })
            .collect(),
    };
    let is_sub_day = |b: &&Bucket| b.span > 0 && b.span <= 86_400;
    let row = |label: &str, b: &Bucket| CcWindow {
        label: label.to_string(),
        util: b.util,
        reset: b.reset,
        span_secs: if b.span > 0 { b.span } else { WEEK_SECS },
    };
    let weekly_named = |n: &str| buckets.iter().find(|b| !is_sub_day(&b) && b.name == n);

    let mut out = Vec::new();
    if let Some(b) = buckets.iter().find(is_sub_day) {
        out.push(row("5H", b));
    }
    if let Some(b) = weekly_named("SPARK") {
        out.push(row("SPARK", b));
    }
    // The account-wide weekly has no name of its own in the feed; on
    // this account it is what the default model spends, which is Astra.
    let reset_7d = num_field(obj, "reset_7d").map(|t| t as i64);
    out.push(CcWindow {
        label: "ASTRA".into(),
        util: num_field(obj, "utilization_7d").unwrap_or(0.0),
        reset: reset_7d,
        span_secs: WEEK_SECS,
    });
    if let Some(b) = weekly_named("RESERVE") {
        out.push(row("RESERVE", b));
    }
    out.extend(
        buckets
            .iter()
            .filter(|b| !is_sub_day(&b) && b.name != "SPARK" && b.name != "RESERVE")
            .map(|b| row(&b.name, b)),
    );
    out
}

/// Parse the Codex feed.  Same scanner as the Claude one — the schema
/// is the same shape, the reduction to windows is what differs.
pub fn parse_codex(body: &str) -> Option<CodexUsage> {
    let generated_at = str_field(body, "generated_at")
        .and_then(parse_iso_utc)
        .unwrap_or(0);
    let accounts_start = body.find("\"accounts\"")?;
    let arr_start = body[accounts_start..].find('[')? + accounts_start;
    let mut accounts: Vec<CxAccount> = objects_in_array(&body[arr_start..])
        .into_iter()
        .map(|obj| CxAccount {
            name: str_field(obj, "name").unwrap_or_default(),
            email: str_field(obj, "email").unwrap_or_default(),
            status: str_field(obj, "status").unwrap_or_default(),
            windows: codex_windows(obj),
        })
        .collect();
    if accounts.is_empty() {
        return None;
    }
    accounts.sort_by(|a, b| natural_cmp(&a.name, &b.name).then_with(|| a.email.cmp(&b.email)));
    Some(CodexUsage { generated_at, accounts })
}

#[cfg(test)]
mod tests {

    /// Verbatim shape of `codex-usage.json` on 2026-09-08: no
    /// account-wide 5h at all (`null`, not 0), one bucket metering a
    /// 5h AND a weekly with the span appended to its label, and a
    /// second bucket metering only a weekly.
    const CODEX_FEED: &str = r#"{
  "generated_at": "2026-09-07T15:33:21.739714+00:00",
  "accounts": [
    {
      "name": "Codex 1",
      "email": "lihao@golia.jp",
      "status": "allowed",
      "utilization_5h": null,
      "utilization_7d": 0.43,
      "reset_5h": null,
      "reset_7d": 1789219017,
      "model_limits": [
        {
          "label": "gpt-reserve",
          "utilization": 0.02,
          "reset": 1789219014,
          "window_minutes": 10080,
          "severity": "normal",
          "is_active": false
        },
        {
          "label": "GPT-5.3-Codex-Spark 5h",
          "utilization": 0.11,
          "reset": 1788813201,
          "window_minutes": 300,
          "severity": "normal",
          "is_active": false
        },
        {
          "label": "GPT-5.3-Codex-Spark 7d",
          "utilization": 0.5,
          "reset": 1789219014,
          "window_minutes": 10080,
          "severity": "normal",
          "is_active": false
        }
      ],
      "credits": { "enabled": false, "unlimited": false, "balance": "0", "reset_credits": 2 },
      "tier": "pro",
      "collected_at": "2026-09-07T15:33:21.738697+00:00"
    }
  ]
}"#;

    #[test]
    fn the_codex_feed_reduces_to_four_rows_in_reading_order() {
        let u = parse_codex(CODEX_FEED).expect("a feed with one account parses");
        assert_eq!(u.accounts.len(), 1);
        let a = &u.accounts[0];
        assert_eq!(a.name, "Codex 1");
        assert_eq!(a.status, "allowed");
        let labels: Vec<&str> = a.windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["5H", "SPARK", "ASTRA", "RESERVE"]);
        let utils: Vec<f64> = a.windows.iter().map(|w| w.util).collect();
        assert_eq!(utils, [0.11, 0.5, 0.43, 0.02]);
        // Each window keeps its OWN reset and its own length — the 5h
        // row is not the weekly's bar drawn short.
        assert_eq!(a.windows[0].reset, Some(1788813201));
        assert_eq!(a.windows[0].span_secs, 300 * 60);
        assert_eq!(a.windows[1].reset, Some(1789219014));
        assert_eq!(a.windows[1].span_secs, WEEK_SECS);
        assert_eq!(a.windows[2].reset, Some(1789219017), "the account's own weekly");
    }

    /// The span a two-window bucket carries is not part of its name.
    /// It once was, and the row came out called `SPARK 7D` while the
    /// lookup for `SPARK` missed and pushed it to the end of the card.
    #[test]
    fn a_window_span_is_not_part_of_the_model_name() {
        assert_eq!(codex_model_name("GPT-5.3-Codex-Spark"), "SPARK");
        assert_eq!(codex_model_name("GPT-5.3-Codex-Spark 5h"), "SPARK");
        assert_eq!(codex_model_name("GPT-5.3-Codex-Spark 7d"), "SPARK");
        assert_eq!(codex_model_name("gpt-reserve"), "RESERVE");
        assert_eq!(codex_model_name("GPT-5.5"), "5.5");
    }

    /// A bucket nobody named still gets a row. Silently dropping it is
    /// how a new model's allowance would go unnoticed until it bit.
    #[test]
    fn an_unnamed_bucket_keeps_a_row_of_its_own() {
        let feed = CODEX_FEED.replace(
            "\"label\": \"gpt-reserve\"",
            "\"label\": \"GPT-6-Comet\"",
        );
        let u = parse_codex(&feed).expect("still parses");
        let labels: Vec<&str> = u.accounts[0].windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["5H", "SPARK", "ASTRA", "COMET"]);
    }

    /// An account with no per-model buckets is still an account: it has
    /// its own weekly, and that row must survive on its own.
    #[test]
    fn an_account_with_no_buckets_still_has_its_weekly() {
        let feed = r#"{"generated_at":"2026-09-07T15:33:21+00:00","accounts":[
          {"name":"Codex 9","email":"x@y.z","status":"allowed",
           "utilization_5h":null,"utilization_7d":0.07,
           "reset_5h":null,"reset_7d":1789219017,"model_limits":[]}]}"#;
        let u = parse_codex(feed).expect("parses");
        let w = &u.accounts[0].windows;
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].label, "ASTRA");
        assert_eq!(w[0].util, 0.07);
    }

    /// The Claude side reduces to the same shape, so the panel has one
    /// way to draw a bar: its own two windows, then one row per model.
    #[test]
    fn a_claude_account_reduces_to_the_same_window_shape() {
        let u = parse(NESTED_FEED).expect("the claude fixture parses");
        let w = u.accounts[1].windows();
        assert_eq!(w[0].label, "5H");
        assert_eq!(w[0].span_secs, 5 * 3_600);
        assert_eq!(w[1].label, "7D");
        assert_eq!(w[1].span_secs, WEEK_SECS);
        assert!(w.len() > 2, "the model caps follow the account's own");
        assert_eq!(w[2].label, w[2].label.to_uppercase(), "panel labels are caps");
    }

    use super::*;

    /// Both ends land on a local midnight, and the data stays inside.
    #[test]
    fn the_range_is_bracketed_by_day_boundaries() {
        // 8/1 00:00 → 8/14 12:00 on the dev box: a right edge mid-day,
        // which is the shape that had no rule after it.
        let lo = 1_785_855_600.0;
        let hi = 1_786_676_400.0;
        let (t0, t1) = snap_range_to_local_days(lo, hi);
        assert!(t0 <= lo && t1 >= hi, "the data must stay inside");
        for edge in [t0, t1] {
            let (_, _, h, mi) = local_mdhm(edge as i64);
            assert_eq!((h, mi), (0, 0), "an edge that is not a local midnight");
        }
        assert!(t1 > hi, "a right edge mid-day gains the day after it");
        // …and a range already on the boundary is left alone rather
        // than gaining a whole empty day.
        let (a, b) = snap_range_to_local_days(t0, t1);
        assert_eq!((a, b), (t0, t1));
    }

    /// The 2026-08-07 report: a bar ending at 00:00 on the 8th drew to
    /// the left of the rule labelled `8/8`.
    ///
    /// The rules are labelled by `local_mdhm`, so they have to land on
    /// the instants `local_mdhm` calls midnight.  Asserted against
    /// whatever timezone the test runs in — that is the point: the old
    /// code was correct only at UTC+0.
    #[test]
    fn the_day_grid_lands_on_local_midnight() {
        // A few instants spread across a fortnight, plus one in the
        // small hours where a UTC-aligned grid is furthest off.
        let base = 1_786_114_800; // 2026-08-08 00:00 JST on the dev box
        for offset in [0, 3_600, 47_000, 86_400, 5 * 86_400, 13 * 86_400] {
            let t = base + offset;
            let start = local_day_start(t);
            assert!(start <= t, "day start must not be in the future of t");
            assert!(t - start < 25 * 3_600, "…and must be the same day");
            let (_, _, h, mi) = local_mdhm(start);
            assert_eq!((h, mi), (0, 0), "a day starts at local 00:00");

            let next = next_local_day_start(t);
            assert!(next > t, "the next day start is strictly after t");
            let (_, _, h, mi) = local_mdhm(next);
            assert_eq!((h, mi), (0, 0), "…and is also a local midnight");
            let step = next - start;
            assert!(
                (23 * 3_600..=25 * 3_600).contains(&step),
                "one calendar day, DST included, got {step}s"
            );
        }
    }

    /// Stepping the grid must walk every day exactly once — no
    /// duplicate rule, no skipped date.
    #[test]
    fn stepping_the_grid_visits_each_day_once() {
        let t0 = 1_786_114_800 - 3 * 86_400 + 12_345;
        let t1 = t0 + 10 * 86_400;
        let mut seen = Vec::new();
        let mut t = local_day_start(t0);
        if t < t0 {
            t = next_local_day_start(t);
        }
        while t < t1 {
            seen.push(local_mdhm(t));
            let n = next_local_day_start(t);
            assert!(n > t, "the walk must make progress");
            t = n;
        }
        assert_eq!(seen.len(), 10, "ten days in ten days: {seen:?}");
        let mut uniq = seen.clone();
        uniq.dedup();
        assert_eq!(uniq.len(), seen.len(), "a date drawn twice: {seen:?}");
    }

    /// The per-model caps are in the feed and must survive parsing.
    ///
    /// An account can sit at 7 % of its week and still be shut out of
    /// a model: on the live feed of 2026-08-01, Claude 3 was at 64 %
    /// on 7d and **85 %** on Fable.  The account-level bars cannot say
    /// that, which is why the card carries a row per model.
    #[test]
    fn model_limits_survive_the_parse() {
        let u = parse(NESTED_FEED).expect("feed parses");
        let m = &u.accounts[1].model_limits;
        assert_eq!(m.len(), 1, "one model cap per account in this feed");
        assert_eq!(m[0].label, "Fable");
        assert!((m[0].util - 0.66).abs() < 1e-9);
        assert_eq!(m[0].reset, Some(1785704399));
        // `"reset": null` means the window never started — not epoch 0.
        let untouched = &u.accounts[0].model_limits[0];
        assert_eq!(untouched.reset, None);
        assert_eq!(untouched.util, 0.0);
    }

    /// The live file on this machine, whatever it currently says —
    /// skipped when it is not there (CI, a fresh checkout).
    #[test]
    fn the_live_feed_on_this_machine_parses_every_account() {
        let Some(u) = read() else { return };
        let n = std::fs::read_to_string(feed_path())
            .unwrap()
            .matches("\"collected_at\"")
            .count();
        assert_eq!(u.accounts.len(), n, "one account parsed per account written");
    }

    /// The real feed, verbatim, as of 2026-08-01 — two accounts kept.
    ///
    /// The shape that broke the old scanner: each account now carries a
    /// `model_limits` array of objects and a `credits` object, so the
    /// first `}` after an account's opening brace closes something
    /// *inside* it.
    const NESTED_FEED: &str = r#"{
  "generated_at": "2026-07-31T22:42:33.439858+00:00",
  "accounts": [
    {
      "name": "Claude 1",
      "email": "lihao@golia.jp",
      "status": "allowed",
      "utilization_5h": 0.05,
      "utilization_7d": 0.05,
      "reset_5h": 1785543600,
      "reset_7d": 1786071600,
      "model_limits": [
        {
          "label": "Fable",
          "utilization": 0.0,
          "reset": null,
          "severity": "normal",
          "is_active": false
        }
      ],
      "credits": {
        "enabled": false,
        "ever_enabled": false,
        "user_disabled": false,
        "spend_limit_reached": false,
        "disabled_reason": null,
        "utilization": 0.0,
        "used_minor": 0,
        "limit_minor": null,
        "currency": "USD",
        "exponent": 2
      },
      "tier": "max_20x",
      "collected_at": "2026-07-31T22:42:29.478675+00:00"
    },
    {
      "name": "Claude 2",
      "email": "admin@golia.jp",
      "status": "allowed",
      "utilization_5h": 0.3,
      "utilization_7d": 0.39,
      "reset_5h": 1785544800,
      "reset_7d": 1785704400,
      "model_limits": [
        {
          "label": "Fable",
          "utilization": 0.66,
          "reset": 1785704399,
          "severity": "normal",
          "is_active": true
        }
      ],
      "credits": {
        "enabled": false,
        "ever_enabled": true,
        "user_disabled": false,
        "spend_limit_reached": false,
        "disabled_reason": "out_of_credits",
        "utilization": 0.0,
        "used_minor": 0,
        "limit_minor": 20000,
        "currency": "USD",
        "exponent": 2
      },
      "tier": "max_20x",
      "collected_at": "2026-07-31T22:42:31.069053+00:00"
    }
  ]
}
"#;

    /// Four accounts in the feed must be four accounts on screen.
    ///
    /// This shipped: the panel read `CLAUDE ACCOUNTS 1` while the feed
    /// held four, because the scanner mistook an inner `}` for the end
    /// of account 1 and the `]` closing its `model_limits` for the end
    /// of the whole array.  Nothing logged; the other three simply were
    /// not there.
    #[test]
    fn nested_sub_objects_do_not_truncate_the_account_list() {
        let u = parse(NESTED_FEED).expect("feed parses");
        assert_eq!(u.accounts.len(), 2, "both accounts, not just the first");
        assert_eq!(u.accounts[0].email, "lihao@golia.jp");
        assert_eq!(u.accounts[1].email, "admin@golia.jp");
        // Fields still come from the account itself, not from a nested
        // object that happens to share a name.
        assert_eq!(u.accounts[1].status, "allowed");
        assert!((u.accounts[1].util_5h - 0.3).abs() < 1e-9);
        assert!((u.accounts[1].util_7d - 0.39).abs() < 1e-9);
        assert_eq!(u.accounts[1].reset_7d, 1785704400);
    }

    /// Accounts are deliberately out of order here, because that is how
    /// the real feed arrives — the collector queries accounts
    /// concurrently and appends each as it answers.
    const SAMPLE: &str = r#"{
  "generated_at": "2026-07-19T01:13:59.715798+00:00",
  "accounts": [
    {
      "name": "Claude 2",
      "email": "admin@golia.jp",
      "status": "allowed",
      "utilization_5h": 0.04,
      "utilization_7d": 0.56,
      "reset_5h": 1784433600,
      "reset_7d": 1784494800,
      "tier": "max_20x",
      "collected_at": "2026-07-19T01:13:57.192408+00:00"
    },
    {
      "name": "Claude 1",
      "email": "lihao@golia.jp",
      "status": "allowed",
      "utilization_5h": 0.0,
      "utilization_7d": 0.39,
      "reset_5h": 1784429400,
      "reset_7d": 1784862000,
      "tier": "max_20x",
      "collected_at": "2026-07-19T01:13:56.377929+00:00"
    }
  ]
}"#;

    #[test]
    fn parses_real_feed_shape() {
        let u = parse(SAMPLE).expect("parse");
        assert_eq!(u.accounts.len(), 2);
        let a = &u.accounts[0];
        assert_eq!(a.name, "Claude 1");
        assert_eq!(a.email, "lihao@golia.jp");
        assert_eq!(a.status, "allowed");
        assert_eq!(a.util_7d, 0.39);
        assert_eq!(a.reset_5h, 1_784_429_400);
        let b = &u.accounts[1];
        assert_eq!(b.email, "admin@golia.jp");
        assert_eq!(b.util_5h, 0.04);
    }

    #[test]
    fn iso_parse_matches_known_unix() {
        // 2026-07-19T01:13:59Z — cross-checked against date(1).
        let u = parse(SAMPLE).unwrap();
        assert_eq!(u.generated_at, 1_784_423_639);
    }

    #[test]
    fn status_kinds_classify_and_label() {
        use CcStatusKind::*;
        assert_eq!(CcStatusKind::classify("allowed"), Ok);
        assert_eq!(CcStatusKind::classify("allowed_warning"), Warn);
        assert_eq!(CcStatusKind::classify("rejected"), Limited);
        assert_eq!(CcStatusKind::classify("no_token"), NoToken);
        assert_eq!(CcStatusKind::classify("unknown"), Other);
        assert_eq!(CcStatusKind::classify(""), Other);
        assert_eq!(Ok.label("allowed"), "ok");
        assert_eq!(Warn.label("allowed_warning"), "near");
        assert_eq!(Limited.label("rejected"), "limit");
        // Unknown values are surfaced but bounded to the reserved slot.
        let long = Other.label("some_unexpected_future_value");
        assert_eq!(long, "some_un");
        assert!(long.chars().count() <= 7);
    }

    /// Names end in an index, so ordering must be numeric.  Plain
    /// string ordering puts "Claude 10" between 1 and 2 — invisible at
    /// four accounts, wrong at ten.
    #[test]
    fn accounts_order_numerically_not_lexicographically() {
        let feed = |names: &[&str]| {
            let objs: Vec<String> = names
                .iter()
                .map(|n| format!("{{\"name\": \"{n}\", \"email\": \"a@b\"}}"))
                .collect();
            format!("{{\"accounts\": [{}]}}", objs.join(","))
        };
        let u = parse(&feed(&["Claude 10", "Claude 2", "Claude 1", "Claude 9"])).unwrap();
        let got: Vec<&str> = u.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(got, ["Claude 1", "Claude 2", "Claude 9", "Claude 10"]);

        // Zero-padding is presentation, not magnitude.
        let u = parse(&feed(&["Claude 03", "Claude 1"])).unwrap();
        let got: Vec<&str> = u.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(got, ["Claude 1", "Claude 03"]);
    }

    #[test]
    fn garbage_and_empty_yield_none() {
        assert!(parse("").is_none());
        assert!(parse("{}").is_none());
        assert!(parse("{\"accounts\": []}").is_none());
        assert!(parse("not json at all").is_none());
    }
}
