//! The agent-accounts panel: the data a frame of it is drawn from,
//! its own measurements, and what the pointer is on.
//!
//! The frame and header are the shared panel chrome's.  Where the
//! cards and the timeline go is `cc_usage_layout`; drawing them is
//! `cc_usage_paint`.

use std::fmt::Write;

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
    use crate::ui::strings::Msg;
    use crate::ui::theme::spec::{card, space};
    use crate::ui::theme::PanelText;

    /// The panel's name, in the header.
    ///
    /// "Agent", not "Claude": the panel carries every provider whose
    /// feed is on disk, and naming it after one of them is how a reader
    /// concludes the other is missing rather than absent.
    pub const TITLE: Msg = Msg::PanelAccounts;
    pub const ACCOUNTS_HEADING: Msg = Msg::AccountsHeading;
    pub const TIMELINE_HEADING: Msg = Msg::AccountsAvailability;
    /// Shown instead of the cards while neither feed has been read.
    pub const EMPTY_TEXT: Msg = Msg::AccountsEmpty;

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
pub fn timeline_range(now: i64, extents: impl IntoIterator<Item = (i64, i64)>) -> (f64, f64) {
    let mut lo = now as f64;
    let mut hi = now as f64;
    for (start, end) in extents {
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
#[derive(Debug, PartialEq)]
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
    /// One line per kind of stored reset: kind, how many are left and
    /// when they can be used.  Empty for an account with none.
    pub resets: Vec<crate::agent_resets::ResetLine>,
}

/// One metered window of an account.
#[derive(Debug, PartialEq)]
pub struct CcUsageWindowRender {
    /// Row label as printed, lower case ("5h", "fable", "spark").
    pub label: String,
    /// 0.0 ..= 1.0 utilization of this window; `None` when there is no
    /// reading, which is drawn as a dash over an empty track, never 0%.
    pub util: Option<f32>,
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

// Cloned by hand so that a kept copy is brought up to date in the
// strings it already has.
impl Clone for CcUsageAccountRender {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            email: self.email.clone(),
            status_label: self.status_label.clone(),
            status_severity: self.status_severity,
            reset_label: self.reset_label.clone(),
            windows: self.windows.clone(),
            resets: self.resets.clone(),
        }
    }

    fn clone_from(&mut self, from: &Self) {
        self.name.clone_from(&from.name);
        self.email.clone_from(&from.email);
        self.status_label.clone_from(&from.status_label);
        self.status_severity = from.status_severity;
        self.reset_label.clone_from(&from.reset_label);
        self.windows.clone_from(&from.windows);
        self.resets.clone_from(&from.resets);
    }
}

impl Clone for CcUsageWindowRender {
    fn clone(&self) -> Self {
        Self {
            label: self.label.clone(),
            util: self.util,
            reset_unix: self.reset_unix,
            span_secs: self.span_secs,
            reset_hm: self.reset_hm.clone(),
        }
    }

    fn clone_from(&mut self, from: &Self) {
        self.label.clone_from(&from.label);
        self.reset_hm.clone_from(&from.reset_hm);
        (self.util, self.reset_unix, self.span_secs) = (from.util, from.reset_unix, from.span_secs);
    }
}

impl CcUsageWindowRender {
    /// The reading as the card and the timeline print it: `42%`, or a
    /// dash where there is none.
    #[cfg(test)]
    pub fn percent(&self) -> String {
        let mut out = String::new();
        self.write_percent(&mut out);
        out
    }

    /// [`Self::percent`] appended to `out`.
    pub fn write_percent(&self, out: &mut impl Write) {
        let _ = match self.util {
            Some(u) => write!(out, "{:.0}%", u * 100.0),
            None => write!(out, "—"),
        };
    }

    /// What is written right of this window's timeline bar:
    /// `7d 100% 12:00`.
    #[cfg(test)]
    pub fn tag(&self) -> String {
        let mut out = String::new();
        self.write_tag(&mut out);
        out
    }

    /// [`Self::tag`] appended to `out`.
    pub fn write_tag(&self, out: &mut impl Write) {
        let _ = write!(out, "{} ", self.label);
        self.write_percent(out);
        let _ = write!(out, " {}", self.reset_hm);
    }

    /// How many characters [`Self::tag`] is.
    pub fn tag_cols(&self) -> usize {
        struct Count(usize);
        impl Write for Count {
            fn write_str(&mut self, s: &str) -> std::fmt::Result {
                self.0 += s.chars().count();
                Ok(())
            }
        }
        let mut n = Count(0);
        self.write_tag(&mut n);
        n.0
    }
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
    /// Where the pointer is, for a scene drawn without anyone having
    /// followed the pointer over the surface.
    pub pointer: Option<(f64, f64)>,
    pub surface: std::rc::Rc<std::cell::RefCell<crate::ui::components::cc_usage_view::CcUsageSurface>>,
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
    cell_w: f64,
    cell_h: f64,
    top_inset: f64,
) -> Rect {
    panel_rect_with((n_accounts, window_rows, 0), w_phys, h_phys, cell_w, cell_h, top_inset)
}

/// [`panel_rect`] for cards that also list up to `reset_rows` lines of
/// stored resets.
pub fn panel_rect_with(
    (n_accounts, window_rows, reset_rows): (usize, usize, usize),
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
    let content_h = usage_layout::content_height(content_w, cell_h, n_accounts, (window_rows, reset_rows));
    panel_chrome::frame_rect(w_phys, h_phys, w, panel_chrome::frame_height(content_h), top_inset)
}

/// Clip `s` to `cols` monospace columns, marking the cut with `…`.
/// Returns empty when there isn't room for at least a few glyphs —
/// a lone ellipsis carries no information and just adds noise.
#[cfg(test)]
pub(crate) fn fit_ellipsis(s: &str, cols: f64) -> String {
    let mut out = s.to_string();
    cut_to_columns(&mut out, cols);
    out
}

/// [`fit_ellipsis`] of what `s` holds, left in `s`.
pub(crate) fn cut_to_columns(s: &mut String, cols: f64) {
    let fit = cols.floor().max(0.0) as usize;
    if s.chars().count() <= fit {
        return;
    }
    if fit < 3 {
        return s.clear();
    }
    let end = s.char_indices().nth(fit - 1).map_or(s.len(), |(at, _)| at);
    s.truncate(end);
    s.push('…');
}

/// Clip `s` to `max_w` as `measure` measures it, marking the cut with
/// `…`.  For proportional text, where a character count says nothing
/// about width.  Empty when not even the mark fits.
#[cfg(test)]
pub(crate) fn fit_measured(s: &str, max_w: f64, measure: &mut dyn FnMut(&str) -> f64) -> String {
    let mut out = String::new();
    fit_measured_into(s, max_w, measure, &mut out);
    out
}

/// [`fit_measured`] written into `out`, which is cleared first.
pub(crate) fn fit_measured_into(s: &str, max_w: f64, measure: &mut dyn FnMut(&str) -> f64, out: &mut String) {
    out.clear();
    if measure(s) <= max_w {
        out.push_str(s);
        return;
    }
    // the first `keep` characters and the mark
    fn cut(s: &str, keep: usize, out: &mut String) {
        let end = s.char_indices().nth(keep).map_or(s.len(), |(at, _)| at);
        out.clear();
        out.push_str(&s[..end]);
        out.push('…');
    }
    // A longer prefix is never narrower, so the longest one that fits
    // can be searched for rather than walked down to.
    let (mut fits, mut over) = (0usize, s.chars().count());
    while over - fits > 1 {
        let mid = (fits + over) / 2;
        cut(s, mid, out);
        if measure(out) <= max_w { fits = mid } else { over = mid }
    }
    // The mark straight after a full stop or a space reads as four
    // dots, or as a mark that has come loose from its word.
    let keep = s.chars().take(fits).enumerate().filter(|(_, c)| !matches!(c, '.' | ' ')).last().map_or(0, |(i, _)| i + 1);
    cut(s, keep, out);
    if measure(out) > max_w {
        out.clear();
    }
}

#[cfg(test)]
#[path = "cc_usage_modal_tests.rs"]
mod tests;
