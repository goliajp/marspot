//! One Claude-schema account row of a usage feed, and its reduction to
//! the windows the panel draws.

use super::{model_limits, num_field, parse_iso_utc, str_field};

/// A per-model cap the feed reports alongside the rolling windows.
///
/// The account-level 5h/7d numbers do not cover these: an account can
/// be at 7 % of its week and still be shut out of one model, which is
/// the single most useful thing to know before starting a session.
#[derive(Debug, Clone, PartialEq)]
pub struct CcModelLimit {
    /// Model name as the feed writes it ("Fable").
    pub label: String,
    /// 0.0 ..= 1.0 utilization of that model's own window; `None` when
    /// the feed has no reading for it, which is not a cap with nothing
    /// used.
    pub util: Option<f64>,
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
    /// 0.0 ..= 1.0 of the five-hour window.  The account is held to
    /// that window exactly while the feed gives a reading: `None` is no
    /// limit to count, `Some(0.0)` a limit with nothing used.
    pub util_5h: Option<f64>,
    /// 0.0 ..= 1.0 of the week.  `None` is no reading: how much is
    /// used is not known, which is neither nothing used nor all of it.
    pub util_7d: Option<f64>,
    /// Unix seconds the five-hour window resets; `None` when the feed
    /// gives none, as for a window that has not started.
    pub reset_5h: Option<i64>,
    /// Unix seconds the week resets; `None` when the feed gives none.
    pub reset_7d: Option<i64>,
    /// Per-model caps, in feed order.  Empty on an older feed.
    pub model_limits: Vec<CcModelLimit>,
    /// Unix seconds this row was collected; 0 when the feed does not
    /// say.
    pub collected_at: i64,
    /// Unix seconds the `model_limits` were actually measured, when
    /// they are not this row's own.
    ///
    /// The feed sets it exactly when `model_limits` and `credits` were
    /// carried over from an earlier sample, so its presence is the
    /// statement "these numbers are older than this row". A reader
    /// that ignores it treats a frozen number as a current one: a pane
    /// sat on an account whose model cap had been carried for 94
    /// minutes while the API refused it sixteen times.
    pub limits_as_of: Option<i64>,
    /// Plan tier as the feed writes it (`default_claude_max_20x`);
    /// empty on an older feed.
    pub tier: String,
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
    /// Row label as the panel prints it, lower case — `5h`, `fable`,
    /// `spark`.  One form, so the card's column and the timeline's tag
    /// are the same string rather than two casings of one name.
    pub label: String,
    /// 0.0 ..= 1.0 utilization of this window; `None` when the feed
    /// has no reading for it, which is not a window with nothing used.
    pub util: Option<f64>,
    /// Unix seconds when it resets. `None` = nothing used yet / not
    /// reported; the card still draws the row, the timeline draws no
    /// bar (there is no extent to draw).
    pub reset: Option<i64>,
    /// How long the window is. The bar runs `[reset - span, reset]`.
    pub span_secs: i64,
}

pub(super) const WEEK_SECS: i64 = 7 * 86_400;

impl CcAccount {
    /// One account object of the feed.
    pub(super) fn from_feed(obj: &str) -> CcAccount {
        CcAccount {
            name: str_field(obj, "name").unwrap_or_default(),
            email: str_field(obj, "email").unwrap_or_default(),
            status: str_field(obj, "status").unwrap_or_default(),
            util_5h: num_field(obj, "utilization_5h"),
            util_7d: num_field(obj, "utilization_7d"),
            reset_5h: num_field(obj, "reset_5h").map(|t| t as i64),
            reset_7d: num_field(obj, "reset_7d").map(|t| t as i64),
            model_limits: model_limits(obj),
            collected_at: str_field(obj, "collected_at").and_then(parse_iso_utc).unwrap_or(0),
            limits_as_of: str_field(obj, "limits_as_of").and_then(parse_iso_utc),
            tier: str_field(obj, "tier").unwrap_or_default(),
        }
    }

    /// The account's windows in reading order: its own, then one per
    /// model cap.  A model cap is weekly on this provider.  The 5h row
    /// is there exactly when the feed gave a reading for it; the week
    /// always has a row, without a figure when there is no reading.
    pub fn windows(&self) -> Vec<CcWindow> {
        let mut out = Vec::new();
        if let Some(util) = self.util_5h {
            out.push(CcWindow {
                label: "5h".into(),
                util: Some(util),
                reset: self.reset_5h,
                span_secs: 5 * 3_600,
            });
        }
        out.push(CcWindow {
            label: "7d".into(),
            util: self.util_7d,
            reset: self.reset_7d,
            span_secs: WEEK_SECS,
        });
        out.extend(self.model_limits.iter().map(|m| CcWindow {
            label: m.label.to_ascii_lowercase(),
            util: m.util,
            reset: m.reset,
            span_secs: WEEK_SECS,
        }));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::{parse, parse_codex};

    fn feed(week: &str, reset: &str) -> String {
        format!(
            r#"{{"generated_at":"2026-11-10T05:55:14+00:00","accounts":[
              {{"name":"Claude 1","email":"a@b","status":"allowed",
                "utilization_5h":null,"utilization_7d":{week},"reset_5h":null,"reset_7d":{reset},
                "model_limits":[{{"label":"Fable","utilization":0.5,"reset":1794538799,"window_minutes":10080}}],
                "collected_at":"2026-11-10T05:55:08+00:00"}}]}}"#
        )
    }

    /// `null` is no reading and `0` is a week with nothing used: two
    /// different rows from the feed's text on.
    #[test]
    fn a_null_week_is_no_reading_and_zero_is_one() {
        let unread = &parse(&feed("null", "null")).expect("parses").accounts[0];
        assert_eq!((unread.util_7d, unread.reset_7d), (None, None));
        let fresh = &parse(&feed("0", "null")).expect("parses").accounts[0];
        assert_eq!((fresh.util_7d, fresh.reset_7d), (Some(0.0), None));
        let used = &parse(&feed("0.42", "1794538799")).expect("parses").accounts[0];
        assert_eq!((used.util_7d, used.reset_7d), (Some(0.42), Some(1_794_538_799)));
        // a row that leaves the fields out says as little as one that
        // writes null
        let absent = feed("null", "null").replace(r#""utilization_7d":null,"#, "").replace(r#","reset_7d":null"#, "");
        assert!(!absent.contains("_7d"));
        let absent = &parse(&absent).expect("parses").accounts[0];
        assert_eq!((absent.util_7d, absent.reset_7d), (None, None));
    }

    /// The week keeps its row in the panel with no figure in it, and
    /// the rows after it are untouched.
    #[test]
    fn a_week_without_a_reading_keeps_its_row_and_has_no_figure() {
        let rows = |week: &str| -> Vec<(String, Option<f64>, Option<i64>)> {
            let a = &parse(&feed(week, "null")).expect("parses").accounts[0];
            a.windows().into_iter().map(|w| (w.label, w.util, w.reset)).collect()
        };
        let fable = ("fable".to_string(), Some(0.5), Some(1_794_538_799));
        assert_eq!(rows("null"), [("7d".to_string(), None, None), fable.clone()]);
        assert_eq!(rows("0"), [("7d".to_string(), Some(0.0), None), fable]);
    }

    /// The Codex account's own weekly is read the same way.
    #[test]
    fn a_codex_week_without_a_reading_has_no_figure() {
        let astra = |week: &str| {
            let a = parse_codex(&feed(week, "null")).expect("parses").accounts.remove(0);
            a.windows.into_iter().find(|w| w.label == "astra").expect("the account's own weekly")
        };
        assert_eq!(astra("null").util, None);
        assert_eq!(astra("0").util, Some(0.0));
    }
}
