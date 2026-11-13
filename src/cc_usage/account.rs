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
    /// 0.0 ..= 1.0 utilization of this window.
    pub util: f64,
    /// Unix seconds when it resets. `None` = nothing used yet / not
    /// reported; the card still draws the row, the timeline draws no
    /// bar (there is no extent to draw).
    pub reset: Option<i64>,
    /// How long the window is. The bar runs `[reset - span, reset]`.
    pub span_secs: i64,
}

pub(super) const WEEK_SECS: i64 = 7 * 86_400;

/// The tier of the Ultra plan, formerly Max 20x, which kept the name.
const ULTRA_TIER: &str = "default_claude_max_20x";

impl CcAccount {
    /// One account object of the feed.
    pub(super) fn from_feed(obj: &str) -> CcAccount {
        CcAccount {
            name: str_field(obj, "name").unwrap_or_default(),
            email: str_field(obj, "email").unwrap_or_default(),
            status: str_field(obj, "status").unwrap_or_default(),
            util_5h: num_field(obj, "utilization_5h").unwrap_or(0.0),
            util_7d: num_field(obj, "utilization_7d").unwrap_or(0.0),
            reset_5h: num_field(obj, "reset_5h").unwrap_or(0.0) as i64,
            reset_7d: num_field(obj, "reset_7d").unwrap_or(0.0) as i64,
            model_limits: model_limits(obj),
            collected_at: str_field(obj, "collected_at").and_then(parse_iso_utc).unwrap_or(0),
            limits_as_of: str_field(obj, "limits_as_of").and_then(parse_iso_utc),
            tier: str_field(obj, "tier").unwrap_or_default(),
        }
    }

    /// Does the 5h window limit this account?
    ///
    /// Ultra dropped its 5h limit on 2026-10-30 and the API went on
    /// reporting the window anyway: the next day an account told by
    /// mail that its limit was gone still read 23% of 5h, with
    /// `five_hour` as the binding claim in the rate-limit headers.
    /// The number is still metered but no longer enforced, so on that
    /// tier it says nothing about whether the account has room.
    /// Unverified Ultra accounts keep the limit for a while; a refusal
    /// a pane sees is what catches those.
    pub fn has_five_hour_limit(&self) -> bool {
        self.tier != ULTRA_TIER
    }

    /// The account's windows in reading order: its own, then one per
    /// model cap.  A model cap is weekly on this provider.
    pub fn windows(&self) -> Vec<CcWindow> {
        let mut out = Vec::new();
        if self.has_five_hour_limit() {
            out.push(CcWindow {
                label: "5h".into(),
                util: self.util_5h,
                reset: (self.reset_5h > 0).then_some(self.reset_5h),
                span_secs: 5 * 3_600,
            });
        }
        out.push(CcWindow {
            label: "7d".into(),
            util: self.util_7d,
            reset: (self.reset_7d > 0).then_some(self.reset_7d),
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
