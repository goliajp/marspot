//! Settings panel — the rows, their geometry, and what clicking one
//! does.
//!
//! Same split as the other modals (`cc_usage_modal` is the reference):
//! shape and behaviour here, painting in `render_metal`.  Everything
//! is a multiple of the painter's cell metrics, so the panel tracks
//! font size without a second set of numbers.
//!
//! ## The rows are data
//!
//! A row is a label, a **cost**, and a control.  The cost line is not
//! decoration — it is the reason the row exists.  Every entry in this
//! panel is a decision marspot has chosen not to make for the user,
//! which means every entry has a downside worth naming; a settings
//! panel that lists only the upsides is asking the user to choose
//! blind.  Row text lives next to the row's effect for exactly that
//! reason: they cannot drift apart.
//!
//! Anything with a right answer is not here.  The rules that keep
//! reclamation safe — never the focused pane, never with work in
//! flight — are correctness, and a switch for them would only be a
//! way to break them.

use crate::settings::Settings;

/// Multipliers on the painter's cell metrics.
pub mod metric {
    /// Line advance, as a multiple of cell height.
    pub const LINE_ADVANCE: f64 = 1.35;
    // The rhythm is the whole layout problem here, so the numbers are
    // written as one scale rather than tuned one at a time.
    //
    // A row is *two lines that belong together* — a label and what it
    // costs — and rows must read as separate from each other.  The
    // first cut spaced them 0.95 and 2.2, a ratio of 2.3, and at that
    // ratio a cost line sits almost as close to the NEXT label as to
    // its own: the eye groups them wrongly and the panel reads as six
    // crowded lines instead of three rows.  Widening the outer gap
    // and tightening the inner one puts the ratio near 3.

    /// Line advance, as a multiple of cell height.
    pub const LINE_ADVANCE_: () = ();
    /// Panel margin inside its own frame.
    pub const PANEL_PAD: f64 = 1.7;
    /// Title baseline to the first section heading.
    pub const TITLE_BREAK: f64 = 1.9;
    /// Gap under a section heading, before its first row.
    pub const HEADING_GAP: f64 = 1.35;
    /// Gap between one section's last cost line and the next heading.
    pub const SECTION_BREAK: f64 = 2.1;
    /// Label baseline to its own cost line.  Tight: they are one unit.
    pub const COST_ADVANCE: f64 = 0.9;
    /// Cost line to the next row's label.  Nearly three times the
    /// inner gap, so the grouping is unambiguous.
    pub const ROW_GAP: f64 = 1.7;
    /// Last cost line to the footer path.
    pub const FOOTER_BREAK: f64 = 2.0;
    /// Height of a control, as a multiple of cell height.
    pub const CONTROL_H: f64 = 1.5;
    /// Padding inside a segment, in cell widths, each side.  Segments
    /// are sized to their own label — equal thirds made `Never` spill
    /// out of its button while `1h` swam in one.
    pub const SEGMENT_PAD: f64 = 1.1;
    /// Gap between segments.
    pub const SEGMENT_GAP: f64 = 0.4;
    /// Toggle width, as a multiple of its height.
    pub const TOGGLE_ASPECT: f64 = 1.85;
}

/// Which setting a row edits.  One variant per row — the panel has no
/// generic "key" plumbing on purpose: a typo'd key would compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    ReclaimEnabled,
    ReclaimIdleMinutes,
    ReclaimPrefetch,
    CircledWide,
}

/// A row's control shape.
pub enum Control {
    /// On / off.
    Toggle(bool),
    /// One of N, with the chosen index.
    Segmented { options: &'static [&'static str], chosen: usize },
}

/// The minutes offered by the idle-threshold row, in order.
///
/// `0` is "never" and is offered as a value rather than only as the
/// switch above it: a user who is turning reclamation off by dragging
/// the number down should find the end of the range where they expect
/// it, not have to notice a different control.
pub const IDLE_CHOICES: &[u32] = &[15, 30, 60, 120, 0];
pub const IDLE_LABELS: &[&str] = &["15m", "30m", "1h", "2h", "Never"];

pub struct RowSpec {
    pub row: Row,
    pub label: &'static str,
    /// What it costs.  Present on every row; see the module note.
    pub cost: &'static str,
}

pub struct Section {
    pub heading: &'static str,
    pub rows: &'static [RowSpec],
}

/// English, like every other panel in the app — and the cost lines
/// are set in the terminal font, where a proportional CJK string sat
/// on mono cells and came out visibly loose.
pub const SECTIONS: &[Section] = &[
    Section {
        heading: "IDLE RECLAMATION",
        rows: &[
            RowSpec {
                row: Row::ReclaimEnabled,
                label: "Reclaim idle claude panes",
                cost: "coming back to one costs ~3s while its session reloads",
            },
            RowSpec {
                row: Row::ReclaimIdleMinutes,
                label: "Idle for",
                cost: "measured by the session transcript's age, not terminal quiet",
            },
            RowSpec {
                row: Row::ReclaimPrefetch,
                label: "Warm up on return",
                cost: "wakes parked panes one per second as you come back",
            },
        ],
    },
    Section {
        heading: "TEXT",
        rows: &[RowSpec {
            row: Row::CircledWide,
            label: "Circled digits take two cells",
            // The line this whole day bought.  Shipped as a default
            // once, reverted within the hour — so it is offered with
            // what it costs written next to it, and off.
            cost: "sized like CJK, but moves the wrap point: text can strand",
        }],
    },
];

impl Row {
    /// This row's control, given the settings in force.
    pub fn control(self, s: &Settings) -> Control {
        match self {
            Row::ReclaimEnabled => Control::Toggle(s.reclaim_enabled),
            Row::ReclaimIdleMinutes => Control::Segmented {
                options: IDLE_LABELS,
                chosen: IDLE_CHOICES
                    .iter()
                    .position(|m| *m == s.reclaim_idle_minutes)
                    // A hand-edited value the panel has no button for
                    // (`= 45`) must not be silently rounded to one it
                    // does.  Nothing is shown as chosen, and only an
                    // actual click changes it.
                    .unwrap_or(usize::MAX),
            },
            Row::ReclaimPrefetch => Control::Toggle(s.reclaim_prefetch),
            Row::CircledWide => Control::Toggle(s.appearance_circled_wide),
        }
    }

    /// Is this row greyed out?
    ///
    /// The two rows under the switch describe *how* reclamation
    /// behaves; with it off they describe nothing.  Shown rather than
    /// hidden so the panel does not change height under the cursor.
    pub fn disabled_by(self, s: &Settings) -> bool {
        match self {
            // A different section: reclamation being off says nothing
            // about how text is drawn.
            Row::ReclaimEnabled | Row::CircledWide => false,
            Row::ReclaimIdleMinutes | Row::ReclaimPrefetch => !s.reclaim_enabled,
        }
    }

    /// Apply a click on segment `seg` (0 for a toggle).  Returns the
    /// settings that result, or `None` when the click changes nothing
    /// — the caller uses that to skip the disk write.
    pub fn apply(self, s: &Settings, seg: usize) -> Option<Settings> {
        if self.disabled_by(s) {
            return None;
        }
        let mut next = s.clone();
        match self {
            Row::ReclaimEnabled => next.reclaim_enabled = !s.reclaim_enabled,
            Row::ReclaimPrefetch => next.reclaim_prefetch = !s.reclaim_prefetch,
            Row::CircledWide => next.appearance_circled_wide = !s.appearance_circled_wide,
            Row::ReclaimIdleMinutes => {
                next.reclaim_idle_minutes = *IDLE_CHOICES.get(seg)?;
            }
        }
        (next != *s).then_some(next)
    }
}

/// Every row, flattened in draw order.
pub fn rows() -> impl Iterator<Item = &'static RowSpec> {
    SECTIONS.iter().flat_map(|s| s.rows.iter())
}

/// Lines of panel chrome: title, the footer path, and the padding
/// above and below them.  Derived height uses this so "the panel is
/// too short" is one edit.
const CHROME_LINES: f64 = 4.4;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Title,
    Heading(usize),
    Row(usize, usize),
    Footer,
}

/// Vertical layout, walked once — the single source for both where
/// things go and how tall the panel is.
///
/// Returns the total height consumed.  `on` is called with every
/// slot's baseline, measured from the panel's top edge.
fn layout_lines(cell_h: f64, mut on: impl FnMut(Slot, f64)) -> f64 {
    let lh = cell_h * metric::LINE_ADVANCE;
    let pad = cell_h * metric::PANEL_PAD;
    let mut y = pad + lh;
    on(Slot::Title, y);
    for (si, section) in SECTIONS.iter().enumerate() {
        y += lh * if si == 0 { metric::TITLE_BREAK } else { metric::SECTION_BREAK };
        on(Slot::Heading(si), y);
        y += lh * metric::HEADING_GAP;
        for ri in 0..section.rows.len() {
            on(Slot::Row(si, ri), y);
            y += lh * (metric::COST_ADVANCE + metric::ROW_GAP);
        }
        // The last row in a section counted a row gap; the section
        // break (or the footer break) replaces it.
        y -= lh * metric::ROW_GAP;
    }
    y += lh * metric::FOOTER_BREAK;
    on(Slot::Footer, y);
    y + pad
}

/// The panel's rect, centred, **sized by what it actually draws**.
///
/// Height comes from the same walk the painter uses, so the two
/// cannot disagree — the first cut guessed it from a constant and
/// left a dead band two rows tall at the bottom.
pub fn panel_rect(
    w_phys: f64,
    h_phys: f64,
    cell_w: f64,
    cell_h: f64,
    top_inset: f64,
) -> marspot_term::layout::Rect {
    // The cost lines are the long text and the labels are short, so
    // they are what sets the panel's width.
    let longest = rows().map(|r| r.cost.chars().count()).max().unwrap_or(48) as f64;
    let w = ((longest + 2.0 * metric::PANEL_PAD + 6.0) * cell_w)
        .max(56.0 * cell_w)
        .min(w_phys * 0.9);
    let h = layout_lines(cell_h, |_, _| {}).min(h_phys * 0.9);
    marspot_term::layout::Rect {
        x: (w_phys - w) / 2.0,
        y_top: ((h_phys - h) / 2.0).max(top_inset + 8.0),
        w,
        h,
    }
}

/// Where each row's pieces land inside the panel.
///
/// **One walker, used by both the painter and the hit-test.**  The
/// alternative — a layout pass and a matching set of coordinates in
/// the click handler — is how a control ends up reacting a row above
/// where it is drawn, and nothing in a screenshot says which of the
/// two is wrong.
pub struct RowGeometry {
    pub row: Row,
    /// Baseline for the label.
    pub label_baseline: f64,
    /// Baseline for the cost line under it.
    pub cost_baseline: f64,
    /// The control's box, right-aligned in the panel.
    pub control: marspot_term::layout::Rect,
}

/// Walk the panel: each heading with its baseline, each row with its
/// geometry, in draw order.  Shares [`layout_lines`] with
/// [`panel_rect`], so what is drawn and how tall the panel is can
/// never disagree.
pub fn walk(
    rect: marspot_term::layout::Rect,
    cell_w: f64,
    cell_h: f64,
    mut heading: impl FnMut(&'static str, f64),
    mut row: impl FnMut(RowGeometry),
) {
    let lh = cell_h * metric::LINE_ADVANCE;
    // The control column is right-aligned on the same edge the text
    // starts from on the left, so the panel has two clean margins.
    let right = rect.x + rect.w - cell_w * metric::PANEL_PAD;
    layout_lines(cell_h, |slot, y| {
        let y = rect.y_top + y;
        match slot {
            Slot::Heading(si) => heading(SECTIONS[si].heading, y),
            Slot::Row(si, ri) => {
                let spec = &SECTIONS[si].rows[ri];
                let h = cell_h * metric::CONTROL_H;
                let w = control_width(spec.row, cell_w, cell_h);
                row(RowGeometry {
                    row: spec.row,
                    label_baseline: y,
                    cost_baseline: y + lh * metric::COST_ADVANCE,
                    control: marspot_term::layout::Rect {
                        x: right - w,
                        // Centred on the label's line, not hung off its
                        // baseline: a control is as tall as two glyphs
                        // and sitting it on the baseline pushes it
                        // into the cost line underneath.
                        y_top: y - cell_h * 0.72 - (h - cell_h) * 0.5,
                        w,
                        h,
                    },
                });
            }
            _ => {}
        }
    });
}

/// How wide this row's control needs to be.
///
/// Sized to content, not to a column constant: `Never` spilled out of
/// an equal-thirds button while `1h` swam in one.
pub fn control_width(row: Row, cell_w: f64, cell_h: f64) -> f64 {
    match row.control(&crate::settings::get()) {
        Control::Toggle(_) => cell_h * metric::CONTROL_H * metric::TOGGLE_ASPECT,
        Control::Segmented { options, .. } => {
            let gaps = cell_w * metric::SEGMENT_GAP * (options.len().saturating_sub(1)) as f64;
            options.iter().map(|l| segment_width(l, cell_w)).sum::<f64>() + gaps
        }
    }
}

fn segment_width(label: &str, cell_w: f64) -> f64 {
    (label.chars().count() as f64 + 2.0 * metric::SEGMENT_PAD) * cell_w
}

/// Baseline for the footer path, from the same walk.
pub fn footer_baseline(rect: marspot_term::layout::Rect, cell_h: f64) -> f64 {
    let mut y = rect.y_top + rect.h;
    layout_lines(cell_h, |slot, at| {
        if slot == Slot::Footer {
            y = rect.y_top + at;
        }
    });
    y
}

/// Left edge for a row's text.
pub fn text_x(rect: marspot_term::layout::Rect, cell_w: f64) -> f64 {
    rect.x + cell_w * metric::PANEL_PAD
}

/// The `n`th segment of a segmented control, sized to its own label.
pub fn segment_rect(
    control: marspot_term::layout::Rect,
    n: usize,
    labels: &[&str],
    cell_w: f64,
) -> marspot_term::layout::Rect {
    let gap = cell_w * metric::SEGMENT_GAP;
    let mut x = control.x;
    for l in labels.iter().take(n) {
        x += segment_width(l, cell_w) + gap;
    }
    marspot_term::layout::Rect {
        x,
        y_top: control.y_top,
        w: labels.get(n).map(|l| segment_width(l, cell_w)).unwrap_or(0.0),
        h: control.h,
    }
}

/// Which row and segment `(px, py)` lands on, if any.
///
/// Walks the same geometry the painter does, so a control cannot be
/// clickable anywhere but where it is drawn.
pub fn hit_test(
    rect: marspot_term::layout::Rect,
    cell_w: f64,
    cell_h: f64,
    px: f64,
    py: f64,
) -> Option<(Row, usize)> {
    let mut hit = None;
    walk(
        rect,
        cell_w,
        cell_h,
        |_, _| {},
        |g| {
            if hit.is_some() {
                return;
            }
            match g.row.control(&crate::settings::get()) {
                Control::Toggle(_) => {
                    if g.control.contains(px, py) {
                        hit = Some((g.row, 0));
                    }
                }
                Control::Segmented { options, .. } => {
                    for n in 0..options.len() {
                        if segment_rect(g.control, n, options, cell_w).contains(px, py) {
                            hit = Some((g.row, n));
                            return;
                        }
                    }
                }
            }
        },
    );
    hit
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The panel's whole justification: every row names its downside.
    /// A row without one is a row that is asking the user to choose
    /// blind, and this catches it at compile-test time rather than in
    /// a screenshot.
    #[test]
    fn every_row_states_what_it_costs() {
        for r in rows() {
            assert!(!r.label.is_empty(), "{:?} has no label", r.row);
            assert!(
                r.cost.len() >= 12,
                "{:?} has no real cost line: {:?}",
                r.row,
                r.cost
            );
        }
        assert!(!SECTIONS.is_empty());
    }

    #[test]
    fn a_toggle_flips_and_a_segment_picks() {
        let s = Settings::default();
        let off = Row::ReclaimEnabled.apply(&s, 0).expect("flips");
        assert!(!off.reclaim_enabled);
        // Same click again comes back.
        assert!(Row::ReclaimEnabled.apply(&off, 0).unwrap().reclaim_enabled);

        let two_hours = Row::ReclaimIdleMinutes.apply(&s, 3).expect("picks");
        assert_eq!(two_hours.reclaim_idle_minutes, 120);
        // Picking what is already chosen writes nothing.
        assert!(Row::ReclaimIdleMinutes.apply(&s, 1).is_none());
        // "从不" is a value in the same control, not a second one.
        let never = Row::ReclaimIdleMinutes.apply(&s, 4).expect("never");
        assert_eq!(never.reclaim_idle_minutes, 0);
        assert_eq!(never.reclaim_after(), None);
    }

    /// With reclamation off, the rows describing how it behaves are
    /// inert — clicking one must not quietly change a value the user
    /// cannot see the effect of.
    #[test]
    fn the_rows_below_the_switch_go_inert_when_it_is_off() {
        let off = Settings { reclaim_enabled: false, ..Settings::default() };
        assert!(Row::ReclaimIdleMinutes.disabled_by(&off));
        assert!(Row::ReclaimPrefetch.disabled_by(&off));
        assert!(!Row::ReclaimEnabled.disabled_by(&off), "the switch itself stays live");
        assert!(Row::ReclaimIdleMinutes.apply(&off, 0).is_none());
        assert!(Row::ReclaimPrefetch.apply(&off, 0).is_none());
        assert!(Row::ReclaimEnabled.apply(&off, 0).is_some());
    }

    /// A value hand-edited to something the panel has no button for
    /// must survive being looked at.  Rounding it to the nearest
    /// offered choice would silently rewrite the user's file the
    /// moment they opened the panel.
    #[test]
    fn a_hand_edited_value_shows_as_nothing_chosen_and_is_not_rounded() {
        let s = Settings { reclaim_idle_minutes: 45, ..Settings::default() };
        match Row::ReclaimIdleMinutes.control(&s) {
            Control::Segmented { chosen, options } => {
                assert!(chosen >= options.len(), "45 must not light up a button");
            }
            _ => panic!("wrong control"),
        }
        // And merely rendering it changed nothing.
        assert_eq!(s.reclaim_idle_minutes, 45);
    }

    /// The claim `walk` is built on: a control is clickable exactly
    /// where it is drawn.  Walk the rows, aim at the middle of each
    /// control the painter would paint, and the hit-test must name
    /// that same row.
    #[test]
    fn every_control_is_clickable_where_it_is_drawn() {
        crate::settings::set_for_test(Settings::default());
        let (cw, ch) = (8.0, 16.0);
        let rect = panel_rect(1200.0, 800.0, cw, ch, 30.0);

        let mut seen: Vec<Row> = Vec::new();
        walk(rect, cw, ch, |_, _| {}, |g| seen.push(g.row));
        assert_eq!(seen.len(), rows().count(), "every row is walked once");

        let mut geo: Vec<RowGeometry> = Vec::new();
        walk(rect, cw, ch, |_, _| {}, |g| geo.push(g));
        for g in &geo {
            match g.row.control(&Settings::default()) {
                Control::Toggle(_) => {
                    let c = g.control;
                    let hit = hit_test(rect, cw, ch, c.x + c.w / 2.0, c.y_top + c.h / 2.0);
                    assert_eq!(hit, Some((g.row, 0)), "toggle {:?}", g.row);
                }
                Control::Segmented { options, .. } => {
                    for n in 0..options.len() {
                        let sr = segment_rect(g.control, n, options, cw);
                        let hit =
                            hit_test(rect, cw, ch, sr.x + sr.w / 2.0, sr.y_top + sr.h / 2.0);
                        assert_eq!(hit, Some((g.row, n)), "segment {n} of {:?}", g.row);
                    }
                }
            }
            // Rows must not overlap each other's controls.
            assert!(
                g.control.x >= rect.x && g.control.x + g.control.w <= rect.x + rect.w + 1e-9,
                "{:?} control escapes the panel",
                g.row
            );
            assert!(
                g.cost_baseline > g.label_baseline,
                "{:?} cost line must sit under its label",
                g.row
            );
        }
        // Every row is inside the panel vertically, cost line included.
        let last = geo.last().unwrap();
        assert!(
            last.cost_baseline < rect.y_top + rect.h,
            "the last row falls outside the panel: {} vs {}",
            last.cost_baseline,
            rect.y_top + rect.h
        );
        // A click in the panel's empty space hits nothing.
        assert_eq!(hit_test(rect, cw, ch, rect.x + 2.0, rect.y_top + 2.0), None);
    }

    /// The two complaints the layout was rebuilt for, as assertions.
    ///
    /// 1. A row's own cost line must sit much closer to its label than
    ///    to the next row's — otherwise the eye groups them wrongly
    ///    and three rows read as six crowded lines.
    /// 2. Nothing may overflow: a segment has to be wide enough for
    ///    its own label (`Never` spilled out of an equal-thirds
    ///    button), and the panel must not end in dead space.
    #[test]
    fn the_rhythm_groups_rows_and_nothing_overflows() {
        crate::settings::set_for_test(Settings::default());
        let (cw, ch) = (8.0, 16.0);
        let rect = panel_rect(1400.0, 900.0, cw, ch, 30.0);

        let mut geo: Vec<RowGeometry> = Vec::new();
        walk(rect, cw, ch, |_, _| {}, |g| geo.push(g));

        for w in geo.windows(2) {
            let inner = w[0].cost_baseline - w[0].label_baseline;
            let outer = w[1].label_baseline - w[0].cost_baseline;
            assert!(
                outer > inner * 1.6,
                "rows do not read as separate: inner {inner:.1} vs outer {outer:.1}"
            );
        }

        for g in &geo {
            // Controls right-align on one edge.
            let right = g.control.x + g.control.w;
            let want = rect.x + rect.w - cw * metric::PANEL_PAD;
            assert!((right - want).abs() < 0.5, "{:?} control off the column", g.row);
            // A segment fits its own label.
            if let Control::Segmented { options, .. } = g.row.control(&Settings::default()) {
                for (n, label) in options.iter().enumerate() {
                    let sr = segment_rect(g.control, n, options, cw);
                    let ink = label.chars().count() as f64 * cw;
                    assert!(
                        sr.w > ink,
                        "{label:?} needs {ink:.1} and its button is {:.1}",
                        sr.w
                    );
                }
                let last = segment_rect(g.control, options.len() - 1, options, cw);
                assert!(
                    last.x + last.w <= g.control.x + g.control.w + 0.5,
                    "the last segment runs past the control"
                );
            }
            // Control sits on the label's line, not over the cost line.
            assert!(
                g.control.y_top + g.control.h < g.cost_baseline,
                "{:?} control overlaps its own cost line",
                g.row
            );
        }

        // No dead band: the footer is near the bottom, and the last
        // row is above it.
        let foot = footer_baseline(rect, ch);
        let bottom = rect.y_top + rect.h;
        assert!(foot < bottom, "footer outside the panel");
        assert!(
            bottom - foot < ch * 3.0,
            "dead space under the footer: {:.1}px",
            bottom - foot
        );
        assert!(geo.last().unwrap().cost_baseline < foot);
    }

    #[test]
    fn the_panel_fits_the_window_it_is_centred_in() {
        for (w, h) in [(1200.0, 800.0), (400.0, 300.0), (3840.0, 2160.0)] {
            let r = panel_rect(w, h, 8.0, 16.0, 30.0);
            assert!(r.w <= w, "wider than the window at {w}x{h}");
            assert!(r.h <= h, "taller than the window at {w}x{h}");
            assert!(r.x >= 0.0 && r.y_top >= 0.0, "off-screen at {w}x{h}");
        }
    }
}
