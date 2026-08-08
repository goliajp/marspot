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
    /// Panel margin inside its own frame.
    pub const PANEL_PAD: f64 = 1.6;
    /// Gap under a section heading.
    pub const HEADING_GAP: f64 = 1.0;
    /// Gap between one section's last row and the next heading.
    pub const SECTION_BREAK: f64 = 1.6;
    /// Baseline advance from a row's label to its cost line.
    pub const COST_ADVANCE: f64 = 1.0;
    /// Advance from one row's label to the next row's label.
    pub const ROW_ADVANCE: f64 = 2.35;
    /// Control column width, in cell widths.
    pub const CONTROL_W: f64 = 22.0;
    /// Height of a control, as a multiple of cell height.
    pub const CONTROL_H: f64 = 1.5;
    /// Gap between the segments of a segmented control.
    pub const SEGMENT_GAP: f64 = 0.35;
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
pub const IDLE_LABELS: &[&str] = &["15 分", "30 分", "1 时", "2 时", "从不"];

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

pub const SECTIONS: &[Section] = &[Section {
    heading: "闲置回收",
    rows: &[
        RowSpec {
            row: Row::ReclaimEnabled,
            label: "回收闲置的 claude pane",
            cost: "回来时那个 pane 要等约 3 秒重新载入会话",
        },
        RowSpec {
            row: Row::ReclaimIdleMinutes,
            label: "闲置多久算闲置",
            cost: "按会话记录的年龄算,不是终端安静的时长",
        },
        RowSpec {
            row: Row::ReclaimPrefetch,
            label: "回到 marspot 时预热",
            cost: "进门就开始唤醒停放的 pane,每秒一个",
        },
    ],
},
Section {
    heading: "文字",
    rows: &[RowSpec {
        row: Row::CircledWide,
        label: "圈圈数字占 2 格",
        // The line this whole day bought.  Shipped as a default once,
        // reverted within the hour — so it is offered with what it
        // costs written next to it, and off.
        cost: "①②③ 跟汉字一样大,但会移动换行点 —— 滚过它的段落可能掉字",
    }],
}];

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
const CHROME_LINES: f64 = 5.6;
/// Lines a section costs beyond its rows: the heading plus its gap.
const SECTION_LINES: f64 = 1.0 + metric::HEADING_GAP + metric::SECTION_BREAK;

/// The panel's rect, centred, derived from what it has to draw.
pub fn panel_rect(
    w_phys: f64,
    h_phys: f64,
    cell_w: f64,
    cell_h: f64,
    top_inset: f64,
) -> marspot_term::layout::Rect {
    let lh = cell_h * metric::LINE_ADVANCE;
    let n_rows = rows().count() as f64;
    let n_sections = SECTIONS.len() as f64;
    let w = (68.0 * cell_w).min(w_phys * 0.9).max(40.0 * cell_w);
    let h = (lh * (CHROME_LINES + n_sections * SECTION_LINES + n_rows * metric::ROW_ADVANCE))
        .min(h_phys * 0.9);
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

/// Walk the panel, calling `f` with each heading baseline and each
/// row's geometry, in draw order.
pub fn walk(
    rect: marspot_term::layout::Rect,
    cell_w: f64,
    cell_h: f64,
    mut heading: impl FnMut(&'static str, f64),
    mut row: impl FnMut(RowGeometry),
) {
    let lh = cell_h * metric::LINE_ADVANCE;
    let pad = cell_h * metric::PANEL_PAD;
    let x_left = rect.x + cell_w * metric::PANEL_PAD;
    let control_w = cell_w * metric::CONTROL_W;
    let control_x = rect.x + rect.w - cell_w * metric::PANEL_PAD - control_w;
    // Title line, then a break before the first heading.
    let mut y = rect.y_top + pad + lh;
    for section in SECTIONS {
        y += lh * metric::SECTION_BREAK;
        heading(section.heading, y);
        y += lh * metric::HEADING_GAP;
        for spec in section.rows {
            let label_baseline = y;
            row(RowGeometry {
                row: spec.row,
                label_baseline,
                cost_baseline: label_baseline + lh * metric::COST_ADVANCE,
                control: marspot_term::layout::Rect {
                    x: control_x,
                    y_top: label_baseline - cell_h * metric::CONTROL_H * 0.75,
                    w: control_w,
                    h: cell_h * metric::CONTROL_H,
                },
            });
            y += lh * metric::ROW_ADVANCE;
        }
    }
    let _ = x_left;
}

/// Left edge for a row's text, matching [`walk`]'s control column.
pub fn text_x(rect: marspot_term::layout::Rect, cell_w: f64) -> f64 {
    rect.x + cell_w * metric::PANEL_PAD
}

/// The `n`th segment of a segmented control inside `control`.
pub fn segment_rect(
    control: marspot_term::layout::Rect,
    n: usize,
    of: usize,
    cell_w: f64,
) -> marspot_term::layout::Rect {
    let of = of.max(1) as f64;
    let gap = cell_w * metric::SEGMENT_GAP;
    let seg_w = (control.w - gap * (of - 1.0)) / of;
    marspot_term::layout::Rect {
        x: control.x + (seg_w + gap) * n as f64,
        y_top: control.y_top,
        w: seg_w,
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
                        if segment_rect(g.control, n, options.len(), cell_w).contains(px, py) {
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
                        let sr = segment_rect(g.control, n, options.len(), cw);
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
