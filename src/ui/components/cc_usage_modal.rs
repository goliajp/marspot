//! The agent-accounts panel: the data a frame of it is drawn from,
//! its own measurements, and what the pointer is on.
//!
//! The frame and header are the shared panel chrome's.  Where the
//! cards and the timeline go is `cc_usage_layout`; drawing them is
//! `cc_usage_paint`.

use marspot_term::layout::Rect;

use crate::ui::components::cc_usage_layout as usage_layout;
use crate::ui::components::panel_chrome::{self, PanelChrome};
use crate::ui::theme::spec;

/// What is this panel's own.  Header, paddings, card look and chip
/// are the shared spec's (`ui::theme::spec`).
///
/// Two units, kept apart by name.  Chrome — insets and the gaps
/// between lines of prose — is in **points**.  Data — the usage bars,
/// their rows, the timeline — is in **terminal cells** (`*_CELLS`),
/// because it is set in the terminal's font and has to keep step with
/// it.
pub mod metric {
    use crate::ui::theme::spec::{card, space};
    use crate::ui::theme::PanelText;

    /// The panel's name, in the header.
    ///
    /// "Agent", not "Claude": the panel carries every provider whose
    /// feed is on disk, and naming it after one of them is how a reader
    /// concludes the other is missing rather than absent.
    pub const TITLE: &str = "Agent accounts";
    pub const ACCOUNTS_HEADING: &str = "Accounts";
    pub const TIMELINE_HEADING: &str = "Availability";
    /// Shown instead of the cards while neither feed has been read.
    pub const EMPTY_TEXT: &str =
        "No usage read yet. Sign in to claude or codex in a pane; this fills in within a few minutes.";

    pub const NAME: PanelText = PanelText::Label;
    pub const EMAIL: PanelText = PanelText::Secondary;
    pub const RESET: PanelText = PanelText::Caption;
    pub const EMPTY: PanelText = PanelText::Secondary;

    /// Narrowest an account card may be before the row of cards wraps:
    /// room for `Claude 10` and a `LIMIT` chip on one line.
    pub const CARD_MIN_W: f64 = 104.0;
    /// A card's inset, the same on all four sides.
    pub const CARD_PAD: f64 = card::ROW_PAD_X;
    /// Least distance from the account name to the chip right of it.
    pub const NAME_TO_CHIP: f64 = space::S8;
    pub const NAME_TO_EMAIL: f64 = space::LABEL_TO_DESC;
    pub const EMAIL_TO_BARS: f64 = space::S8;
    pub const BARS_TO_RESET: f64 = space::S8;
    /// The plot's rows to the `NOW` label above and the dates below.
    pub const PLOT_TO_AXIS: f64 = space::S4;
    /// Height of the content area while there is nothing to show.
    pub const EMPTY_H: f64 = 120.0;
    /// Corner radius of a usage bar.
    pub const BAR_RADIUS: f64 = 1.0;
    /// A bar, or the used part of one, narrower than this many physical
    /// pixels is not drawn: it would be a smudge, not a bar.
    pub const BAR_MIN_PX: f64 = 0.5;
    /// Thickness of the line marking the present.
    pub const NOW_W: f64 = 0.75;
    /// Thickness of a day rule, in physical pixels.
    pub const DAY_RULE_PX: f64 = 1.0;

    /// Usage bar thickness, in cell heights.
    pub const BAR_H_CELLS: f64 = 0.62;
    /// Between two bars of one timeline row, in cell heights: with the
    /// bar itself, more than a line, so two tags never share one.
    pub const TIMELINE_BAR_GAP_CELLS: f64 = 0.78;
    /// A card's percentage column: `100%`.
    pub const PCT_CELLS: f64 = 4.0;
    /// Between a bar and the text either side of it, in cell widths.
    pub const BAR_GAP_CELLS: f64 = 1.0;
    /// Most of the timeline's width the account names, and the tags
    /// right of the last bar, may each take from the plot.
    pub const TIMELINE_SIDE_MAX_FRAC: f64 = 0.3;
    /// Room one date takes on the axis: `11/10` and the gap after it.
    pub const AXIS_LABEL_CELLS: f64 = 7.0;
    /// Day-rule dash and gap lengths, in cell heights.
    pub const GRID_DASH_CELLS: f64 = 0.30;
    pub const GRID_GAP_CELLS: f64 = 0.26;

    /// Utilisation from which a bar is drawn as a warning, and from
    /// which as a problem.
    pub const WARN_AT: f32 = 0.5;
    pub const DANGER_AT: f32 = 0.85;
}

/// Fallback timeline span when there is nothing to measure: ±6 days
/// around now, in seconds.  Real spans come from [`timeline_range`].
pub const TIMELINE_SPAN_SECS: f64 = 12.0 * 86_400.0;

const DAY_SECS: i64 = 86_400;

/// The instants the plot must cover: every bar's full extent, plus now.
///
/// This used to be a fixed ±6 days, which does not fit the data it is
/// drawing: a 7-day window resets up to 7 days out, so its bar ran off
/// the right end and was clamped there — ending at the axis edge
/// instead of at the time written on its own label.
///
/// Deriving the range from the extents makes clamping impossible, which
/// is what lets the label sit immediately right of a bar end that is
/// always the truth.
pub fn timeline_range(now: i64, extents: &[(i64, i64)]) -> (f64, f64) {
    let mut lo = now as f64;
    let mut hi = now as f64;
    for &(start, end) in extents {
        lo = lo.min(start as f64);
        hi = hi.max(end as f64);
    }
    if hi - lo < 3_600.0 {
        // Degenerate feed (everything resets within the hour): fall
        // back to the fixed span rather than draw a plot whose scale
        // magnifies clock skew.
        let n = now as f64;
        return (n - TIMELINE_SPAN_SECS / 2.0, n + TIMELINE_SPAN_SECS / 2.0);
    }
    // No percentage padding: the caller snaps this outward to day
    // boundaries, and that is the margin.
    (lo, hi)
}

/// The present, and the calendar the timeline's days are read in.
///
/// The day rules are labelled with dates, so they have to fall where
/// those dates begin — which depends on the time zone.  The running
/// app uses the machine's; a picture that has to come out the same on
/// every machine names a fixed offset instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    pub now_unix: i64,
    /// Seconds east of UTC.  `None` = the machine's time zone.
    pub utc_offset: Option<i64>,
}

impl Clock {
    pub fn local(now_unix: i64) -> Self {
        Self { now_unix, utc_offset: None }
    }

    pub fn fixed(now_unix: i64, utc_offset: i64) -> Self {
        Self { now_unix, utc_offset: Some(utc_offset) }
    }

    /// The midnight at or before `unix`.
    pub fn day_start(&self, unix: i64) -> i64 {
        match self.utc_offset {
            None => crate::cc_usage::local_day_start(unix),
            Some(off) => (unix + off).div_euclid(DAY_SECS) * DAY_SECS - off,
        }
    }

    /// The midnight strictly after `unix`.
    pub fn next_day_start(&self, unix: i64) -> i64 {
        match self.utc_offset {
            None => crate::cc_usage::next_local_day_start(unix),
            Some(_) => self.day_start(unix) + DAY_SECS,
        }
    }

    /// `(month, day)` of `unix`.
    pub fn month_day(&self, unix: i64) -> (u32, u32) {
        match self.utc_offset {
            None => {
                let (mo, d, _, _) = crate::cc_usage::local_mdhm(unix);
                (mo, d)
            }
            Some(off) => {
                // Days since 1970-01-01 to a civil date, counted from
                // 1 March so the leap day is the last of its year.
                let z = (unix + off).div_euclid(DAY_SECS) + 719_468;
                let doe = z.rem_euclid(146_097);
                let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
                let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
                let mp = (5 * doy + 2) / 153;
                let day = doy - (153 * mp + 2) / 5 + 1;
                let month = if mp < 10 { mp + 3 } else { mp - 9 };
                (month as u32, day as u32)
            }
        }
    }

    /// Widen `[t0, t1]` out to the midnights that bracket it, so every
    /// bar has a dated rule on both sides of it.
    pub fn snap_to_days(&self, t0: f64, t1: f64) -> (f64, f64) {
        let lo = self.day_start(t0 as i64);
        let hi = {
            let s = self.day_start(t1 as i64);
            if (s as f64) < t1 { self.next_day_start(s) } else { s }
        };
        (lo as f64, hi as f64)
    }
}

/// One account card.  Pre-formatted by the caller; the renderer only
/// lays out and paints.
#[derive(Debug, Clone)]
pub struct CcUsageAccountRender {
    pub name: String,
    pub email: String,
    /// Short status label (≤ 7 chars, pre-classified from the feed's
    /// raw status string) + the severity that picks its colour.
    pub status_label: String,
    /// 0 = ok, 1 = warn, 2 = limited/unknown-bad.
    pub status_severity: u8,
    /// "reset: 7/19 11:50" — local time, formatted by the caller.
    pub reset_label: String,
    /// Every window this account is metered on, in reading order.
    ///
    /// Not a fixed 5h/7d pair plus extras: that is Anthropic's shape,
    /// and a Codex account has no account-wide 5h at all — its only
    /// sub-day allowance belongs to one model.  One list means the card
    /// and the timeline draw the same rows, and a provider that meters
    /// something new needs no new field.
    pub windows: Vec<CcUsageWindowRender>,
}

/// One metered window of an account.
#[derive(Debug, Clone)]
pub struct CcUsageWindowRender {
    /// Row label as printed, lower case ("5h", "fable", "spark").
    pub label: String,
    /// 0.0 ..= 1.0 utilization of this window.
    pub util: f32,
    /// Unix reset instant.  `None` = nothing used yet / not reported:
    /// the card still draws the row, the timeline draws no bar, because
    /// there is no extent to draw.
    pub reset_unix: Option<i64>,
    /// Window length in seconds; the timeline bar runs
    /// `[reset - span, reset]`.
    pub span_secs: i64,
    /// "11:50" — the bar's end label.
    pub reset_hm: String,
}

impl CcUsageWindowRender {
    /// What is written right of this window's timeline bar:
    /// `7d 100% 12:00`.
    pub fn tag(&self) -> String {
        format!("{} {:.0}% {}", self.label, self.util * 100.0, self.reset_hm)
    }
}

/// What the pointer is on: nothing the panel reacts to, or the
/// header's close button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hover {
    #[default]
    None,
    Close,
}

/// Everything one frame of the panel is drawn from.
pub struct CcUsageRender {
    /// The panel's frame, from [`panel_rect`].
    pub rect: Rect,
    /// "updated 7/19 10:13" — when the feed was generated.  Empty when
    /// there is no feed.
    pub updated_label: String,
    /// No accounts = no feed read yet; the panel says so instead of
    /// drawing cards.
    pub accounts: Vec<CcUsageAccountRender>,
    pub clock: Clock,
    /// From [`hover_at`].
    pub hover: Hover,
}

/// What `(x, y)` is on in the panel at `rect`.  The painter highlights
/// it and a click acts on it, so the two cannot disagree.
pub fn hover_at(rect: Rect, x: f64, y: f64) -> Hover {
    if PanelChrome::new(rect, 0.0).hit_close(x, y) {
        Hover::Close
    } else {
        Hover::None
    }
}

/// How many accounts the two feeds hold between them, and the most
/// windows any one of them is metered on — the two numbers the panel's
/// size depends on.
pub fn feed_shape(
    claude: Option<&crate::cc_usage::CcUsage>,
    codex: Option<&crate::cc_usage::CodexUsage>,
) -> (usize, usize) {
    let claude = claude.map(|d| d.accounts.as_slice()).unwrap_or_default();
    let codex = codex.map(|d| d.accounts.as_slice()).unwrap_or_default();
    let window_rows = claude
        .iter()
        .map(|a| a.windows().len())
        .chain(codex.iter().map(|a| a.windows.len()))
        .max()
        .unwrap_or(0);
    (claude.len() + codex.len(), window_rows)
}

/// The panel's frame for `n_accounts` accounts metered on up to
/// `window_rows` windows each, in a `w_phys × h_phys` window.
///
/// As wide as a panel may be: the timeline wants the room.  As tall as
/// what it draws, cut to the window's share when that is less.
pub fn panel_rect(
    n_accounts: usize,
    window_rows: usize,
    w_phys: f64,
    h_phys: f64,
    _cell_w: f64,
    cell_h: f64,
    top_inset: f64,
) -> Rect {
    let w = w_phys * spec::panel::MAX_W_FRAC;
    // Only the width matters to the probe: the content's height is a
    // function of how wide it is.
    let content_w = PanelChrome::new(Rect { x: 0.0, y_top: 0.0, w, h: 0.0 }, 0.0).content.w;
    let content_h = usage_layout::content_height(content_w, cell_h, n_accounts, window_rows);
    panel_chrome::frame_rect(w_phys, h_phys, w, panel_chrome::frame_height(content_h), top_inset)
}

/// Clip `s` to `cols` monospace columns, marking the cut with `…`.
/// Returns empty when there isn't room for at least a few glyphs —
/// a lone ellipsis carries no information and just adds noise.
pub(crate) fn fit_ellipsis(s: &str, cols: f64) -> String {
    let fit = cols.floor().max(0.0) as usize;
    if s.chars().count() <= fit {
        return s.to_string();
    }
    if fit < 3 {
        return String::new();
    }
    s.chars().take(fit - 1).chain(['…']).collect()
}

/// Clip `s` to `max_w` as `measure` measures it, marking the cut with
/// `…`.  For proportional text, where a character count says nothing
/// about width.  Empty when not even the mark fits.
pub(crate) fn fit_measured(s: &str, max_w: f64, measure: &mut dyn FnMut(&str) -> f64) -> String {
    if measure(s) <= max_w {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let cut = |keep: usize| -> String { chars[..keep].iter().chain(&['…']).collect() };
    // A longer prefix is never narrower, so the longest one that fits
    // can be searched for rather than walked down to.
    let (mut fits, mut over) = (0usize, chars.len());
    while over - fits > 1 {
        let mid = (fits + over) / 2;
        if measure(&cut(mid)) <= max_w { fits = mid } else { over = mid }
    }
    // The mark straight after a full stop or a space reads as four
    // dots, or as a mark that has come loose from its word.
    let keep = chars[..fits].iter().rposition(|c| !matches!(c, '.' | ' ')).map_or(0, |i| i + 1);
    let out = cut(keep);
    if measure(&out) <= max_w { out } else { String::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_ellipsis_bounds_every_case() {
        // Fits untouched.
        assert_eq!(fit_ellipsis("ada@example.jp", 20.0), "ada@example.jp");
        assert_eq!(fit_ellipsis("abc", 3.0), "abc");
        // Too long: cut to exactly the budget, last column is the mark.
        let cut = fit_ellipsis("ada@example.jp", 8.0);
        assert_eq!(cut, "ada@exa…");
        assert_eq!(cut.chars().count(), 8);
        // Never exceeds the budget for any prefix length.
        for cols in 3..30 {
            let out = fit_ellipsis("some.very.long.address@example.com", cols as f64);
            assert!(out.chars().count() <= cols, "cols={cols} out={out:?}");
        }
        // Below the useful minimum the slot is dropped, not filled with
        // a bare ellipsis.
        assert_eq!(fit_ellipsis("abcdef", 2.0), "");
        assert_eq!(fit_ellipsis("abcdef", 0.0), "");
    }

    /// Uneven widths, like a proportional font: `m` and `w` are wide,
    /// `i`, `l` and `.` narrow.
    fn uneven(s: &str) -> f64 {
        s.chars()
            .map(|c| match c {
                'm' | 'w' | '@' => 11.0,
                'i' | 'l' | '.' => 3.0,
                _ => 7.0,
            })
            .sum()
    }

    #[test]
    fn fit_measured_keeps_the_longest_prefix_that_fits() {
        let email = "some.very.long.address@example.com";
        assert_eq!(fit_measured(email, uneven(email), &mut uneven), email, "fits: untouched");
        for max_w in [20.0, 60.0, 100.0, 150.0, uneven(email) - 0.5] {
            let out = fit_measured(email, max_w, &mut uneven);
            assert!(uneven(&out) <= max_w, "max_w={max_w} out={out:?}");
            assert!(out.ends_with('…'), "a cut has to show: {out:?}");
            let kept = out.trim_end_matches('…');
            assert!(email.starts_with(kept));
            assert!(!kept.ends_with('.'), "the mark must not follow a full stop: {out:?}");
            // One more character would not have fitted — one more
            // that is not a full stop, which is never the last kept.
            let more = email[kept.len()..].find(|c| c != '.').map_or(email.len(), |i| kept.len() + i + 1);
            let longer = format!("{}…", &email[..more]);
            assert!(uneven(&longer) > max_w, "max_w={max_w}: {longer:?} also fits");
        }
        // "some.very." would fit; the cut steps back over the stop.
        assert_eq!(fit_measured(email, uneven("some.very.…"), &mut uneven), "some.very…");
        assert_eq!(fit_measured(email, 7.0, &mut uneven), "…");
        assert_eq!(fit_measured(email, 5.0, &mut uneven), "", "no room for the mark either");
    }

    /// Every bar must fit: the plot exists to show where each window
    /// ends, and a bar clamped at the axis edge says the opposite of
    /// what its own label says.
    ///
    /// Numbers are the live feed of 2026-08-01 (four accounts, both
    /// windows each).  Under the old fixed ±6 d span, Claude 1's and
    /// Claude 4's 7-day bars ended at 1.014 and 1.056 of the span.
    #[test]
    fn every_window_fits_inside_the_range() {
        let now = 1785538544i64;
        let resets_7d = [1786071600i64, 1785704400, 1785646800, 1786114800];
        let resets_5h = [1785543600i64, 1785544800, 1785543600, 1785546000];
        let extents: Vec<(i64, i64)> = resets_5h
            .iter()
            .map(|r| (r - 5 * 3_600, *r))
            .chain(resets_7d.iter().map(|r| (r - 7 * 86_400, *r)))
            .collect();
        let (t0, t1) = timeline_range(now, &extents);
        for (start, end) in extents {
            let f0 = (start as f64 - t0) / (t1 - t0);
            let f1 = (end as f64 - t0) / (t1 - t0);
            assert!(f0 >= 0.0 && f1 <= 1.0, "window {start}..{end} escapes the plot: {f0}..{f1}");
        }
        // Now is inside it too — the marker has to land somewhere real.
        assert!((now as f64) > t0 && (now as f64) < t1);
    }

    /// A feed whose windows all reset within the hour would otherwise
    /// be drawn at a scale where clock skew looks like a day.
    #[test]
    fn a_degenerate_feed_falls_back_to_the_fixed_span() {
        let now = 1785538544i64;
        let (t0, t1) = timeline_range(now, &[(now - 60, now + 60)]);
        assert!((t1 - t0 - TIMELINE_SPAN_SECS).abs() < 1.0);
    }

    /// A fixed offset reads the same instants as the same dates on any
    /// machine.
    #[test]
    fn a_fixed_clock_does_not_ask_the_machine_what_day_it_is() {
        // 2026-11-10 09:00 UTC.
        let now = 1_794_301_200;
        let utc = Clock::fixed(now, 0);
        assert_eq!(utc.month_day(now), (11, 10));
        assert_eq!(utc.day_start(now), now - 9 * 3_600);
        assert_eq!(utc.next_day_start(now), now + 15 * 3_600);
        assert_eq!(utc.day_start(utc.day_start(now)), utc.day_start(now), "midnight is its own day's start");
        // Nine hours east it is 18:00 the same day, and midnight came
        // nine hours sooner.
        let east = Clock::fixed(now, 9 * 3_600);
        assert_eq!(east.month_day(now), (11, 10));
        assert_eq!(east.day_start(now), now - 18 * 3_600);
        assert_eq!(east.month_day(now + 6 * 3_600), (11, 11));
        // Month and year ends, and a leap day.
        assert_eq!(utc.month_day(1_798_761_599), (12, 31)); // 2026-12-31 23:59:59
        assert_eq!(utc.month_day(1_798_761_600), (1, 1));
        assert_eq!(utc.month_day(1_709_164_800), (2, 29)); // 2024-02-29
        assert_eq!(utc.month_day(0), (1, 1));
        let (t0, t1) = utc.snap_to_days(now as f64, (now + 3 * 86_400) as f64);
        assert_eq!((t0, t1), ((now - 9 * 3_600) as f64, (now + 3 * 86_400 + 15 * 3_600) as f64));
        assert_eq!(utc.snap_to_days(t0, t1), (t0, t1), "already on day boundaries: left alone");
    }

    /// With no offset given the days are the machine's, as before.
    #[test]
    fn a_local_clock_agrees_with_the_machines_calendar() {
        let now = 1_794_301_200;
        let local = Clock::local(now);
        assert_eq!(local.day_start(now), crate::cc_usage::local_day_start(now));
        assert_eq!(local.next_day_start(now), crate::cc_usage::next_local_day_start(now));
        let (mo, d, _, _) = crate::cc_usage::local_mdhm(now);
        assert_eq!(local.month_day(now), (mo, d));
        assert_eq!(
            local.snap_to_days(now as f64, (now + 86_400) as f64),
            crate::cc_usage::snap_range_to_local_days(now as f64, (now + 86_400) as f64),
        );
    }

    /// The header is the shared chrome's: its close button answers
    /// where the chrome draws it.
    #[test]
    fn the_close_button_is_hit_where_the_chrome_draws_it() {
        let rect = panel_rect(10, 2, 2800.0, 1300.0, 8.0, 16.0, 30.0);
        let close = PanelChrome::new(rect, 0.0).close;
        let (cx, cy) = (close.x + close.w / 2.0, close.y_top + close.h / 2.0);
        assert_eq!(hover_at(rect, cx, cy), Hover::Close);
        for (x, y) in [
            (close.x + 0.5, close.y_top + 0.5),
            (close.x + close.w - 0.5, close.y_top + close.h - 0.5),
        ] {
            assert_eq!(hover_at(rect, x, y), Hover::Close, "a corner of the button is the button");
        }
        assert_eq!(hover_at(rect, close.x - 4.0, cy), Hover::None, "beside it is the header");
        assert_eq!(hover_at(rect, cx, close.y_top + close.h + 4.0), Hover::None);
        assert_eq!(hover_at(rect, rect.x + rect.w + 4.0, cy), Hover::None, "outside the frame");
    }

    /// The panel takes the widest step a panel has and never leaves
    /// the window, whatever it is asked to hold.
    #[test]
    fn panel_rect_stays_inside_the_window() {
        for n in 0..=12 {
            let r = panel_rect(n, 3, 1600.0, 1000.0, 8.0, 18.0, 60.0);
            assert_eq!(r.w, 1600.0 * spec::panel::MAX_W_FRAC, "n={n}");
            assert!(r.x >= 0.0 && r.x + r.w <= 1600.0, "n={n} overflows width");
            assert!(r.y_top >= 60.0, "n={n} must clear the top inset");
            assert!(r.h <= 1000.0 * spec::panel::MAX_H_FRAC + 0.001, "n={n} too tall");
        }
        // No accounts: the header and the band the sentence sits in.
        let empty = panel_rect(0, 0, 1600.0, 1000.0, 8.0, 18.0, 60.0);
        assert_eq!(empty.h, panel_chrome::frame_height(spec::px(metric::EMPTY_H)));
    }
}
