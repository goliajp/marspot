//! cc — Claude profile usage feed for the toolbar `Cc` modal.
//!
//! The devops side drops a small JSON snapshot at
//! `~/.local/state/devops/claude-usage.json` (one object per Claude
//! account: rolling 5-hour and 7-day window utilization + the unix
//! reset instants).  This module reads + parses that file with a
//! purpose-built scanner — the schema is fixed and tiny, a JSON
//! crate would be a dependency for one file (self-build principle).
//!
//! Everything here is read-only and cold-path: the file is touched
//! only while the modal is open (open + 5 s refresh), never on the
//! render hot path.

use std::path::PathBuf;

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
    let mut accounts = Vec::new();
    let mut rest = &body[arr_start + 1..];
    loop {
        let Some(obj_start) = rest.find('{') else { break };
        // Objects in this feed are flat — the next '}' closes it.
        let Some(obj_end) = rest[obj_start..].find('}') else { break };
        let obj = &rest[obj_start..obj_start + obj_end + 1];
        accounts.push(CcAccount {
            name: str_field(obj, "name").unwrap_or_default(),
            email: str_field(obj, "email").unwrap_or_default(),
            status: str_field(obj, "status").unwrap_or_default(),
            util_5h: num_field(obj, "utilization_5h").unwrap_or(0.0),
            util_7d: num_field(obj, "utilization_7d").unwrap_or(0.0),
            reset_5h: num_field(obj, "reset_5h").unwrap_or(0.0) as i64,
            reset_7d: num_field(obj, "reset_7d").unwrap_or(0.0) as i64,
        });
        rest = &rest[obj_start + obj_end + 1..];
        // Stop at the array's closing bracket (an object brace can't
        // appear before it in this flat schema).
        if let (Some(bracket), next_obj) = (rest.find(']'), rest.find('{')) {
            match next_obj {
                Some(o) if o < bracket => continue,
                _ => break,
            }
        } else {
            break;
        }
    }
    if accounts.is_empty() {
        return None;
    }
    Some(CcUsage { generated_at, accounts })
}

/// `"key": "value"` string extractor.
fn str_field(obj: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let at = obj.find(&pat)? + pat.len();
    let colon = obj[at..].find(':')? + at;
    let open = obj[colon..].find('"')? + colon;
    let close = obj[open + 1..].find('"')? + open + 1;
    Some(obj[open + 1..close].to_string())
}

/// `"key": 12.34` number extractor.
fn num_field(obj: &str, key: &str) -> Option<f64> {
    let pat = format!("\"{key}\"");
    let at = obj.find(&pat)? + pat.len();
    let colon = obj[at..].find(':')? + at;
    let tail = obj[colon + 1..].trim_start();
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

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
  "generated_at": "2026-07-19T01:13:59.715798+00:00",
  "accounts": [
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
    },
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

    #[test]
    fn garbage_and_empty_yield_none() {
        assert!(parse("").is_none());
        assert!(parse("{}").is_none());
        assert!(parse("{\"accounts\": []}").is_none());
        assert!(parse("not json at all").is_none());
    }
}
