//! Settings panel — the rows, their geometry, and what clicking one
//! does.
//!
//! Same split as the other modals (`cc_usage_modal` is the reference):
//! shape and behaviour here, painting in `render_metal`.  The layout
//! is a list of cards, each holding rows separated by a hairline —
//! the shape the OS uses for exactly this job, and the shape a flat
//! stack of same-sized lines could never be spaced into.
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

/// Every measurement in this panel, in **typographic points**.
///
/// The first cut expressed the panel as multiples of the terminal's
/// cell so it would track font size.  That coupling is what made it
/// look wrong: a cell is one size, so every line came out one size,
/// and a settings panel that cannot set a label apart from the
/// sentence explaining it has no hierarchy at all.  Chrome is chrome —
/// it is sized like the rest of the system's chrome, in points, and
/// [`crate::ui::core::ViewPainter::px_per_pt`] takes it to pixels.
pub mod metric {
    use crate::ui::theme::PanelText;

    /// Panel width.  Sized for the longest cost line plus the widest
    /// control on the same row, with room to spare.
    pub const PANEL_W: f64 = 440.0;
    /// Panel margin, left and right.
    pub const PAD_X: f64 = 18.0;
    pub const PAD_TOP: f64 = 16.0;
    pub const PAD_BOTTOM: f64 = 14.0;

    /// Title baseline to the first group heading's baseline.
    pub const TITLE_TO_GROUP: f64 = 20.0;
    /// Group baseline to the top of its card.
    pub const GROUP_TO_CARD: f64 = 7.0;

    /// The card holding a group's rows.
    pub const CARD_RADIUS: f64 = 7.0;
    /// Row inset inside the card.
    pub const CARD_PAD_X: f64 = 12.0;
    /// Card bottom to the next group's baseline.
    pub const CARD_TO_GROUP: f64 = 18.0;
    /// Card bottom to the footer baseline's line box.
    pub const CARD_TO_FOOTER: f64 = 16.0;

    /// Row padding above the title line and below the cost line.
    pub const ROW_PAD_Y: f64 = 8.0;
    /// The title line's height — sized to the tallest control so every
    /// row in a card is the same height whatever it holds.  A card
    /// whose rows jump between two heights reads as broken.
    pub const ROW_BAND_H: f64 = 16.0;
    /// Title line to the cost line.
    pub const DESC_GAP: f64 = 3.0;
    /// Hairline between rows, in physical px — one device pixel, which
    /// is what a hairline is.
    pub const SEP_H: f64 = 1.0;

    // Type comes from the shared panel ladder, never from numbers
    // here: the settings panel's labels used to be set at the size of
    // every other panel's *title*, which is the whole reason
    // `PanelText` exists.
    pub const TITLE: PanelText = PanelText::Title;
    pub const GROUP: PanelText = PanelText::Section;
    pub const LABEL: PanelText = PanelText::Label;
    pub const DESC: PanelText = PanelText::Secondary;
    pub const FOOTER: PanelText = PanelText::Caption;
    pub const SEGMENT: PanelText = PanelText::Secondary;

    /// Switch, at the system's proportions.
    pub const TOGGLE_H: f64 = 12.0;
    pub const TOGGLE_W: f64 = 21.0;
    /// Segmented control.  Each segment is sized to its own measured
    /// label — equal thirds spilled `Never` out of its button.
    pub const SEG_H: f64 = 16.0;
    pub const SEG_PAD_X: f64 = 8.0;
    pub const SEG_GAP: f64 = 3.0;
}

/// Which setting a row edits.  One variant per row — the panel has no
/// generic "key" plumbing on purpose: a typo'd key would compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    ReclaimEnabled,
    ReclaimIdleMinutes,
    ReclaimPrefetch,
    DimScale,
    CircledWide,
    ScrollFactor,
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

/// How far an inactive pane steps back, as a multiplier on the
/// attention ladder.  `0` is off — offered as a value rather than a
/// second switch, same reasoning as `Never` above.
pub const DIM_CHOICES: &[f32] = &[0.0, 0.6, 1.0, 1.4];
pub const DIM_LABELS: &[&str] = &["Off", "Light", "Normal", "Deep"];

/// Wheel / trackpad multiplier.
pub const SCROLL_CHOICES: &[f32] = &[0.5, 1.0, 1.5, 2.5];
pub const SCROLL_LABELS: &[&str] = &["Slow", "Normal", "Fast", "Faster"];

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

/// English, like every other panel in the app.  Group headings are
/// sentence case, not shouted: they sit above their card at 12pt
/// semibold, where all-caps only adds noise.
pub const SECTIONS: &[Section] = &[
    Section {
        heading: "Idle reclamation",
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
        heading: "Appearance",
        rows: &[
            RowSpec {
                row: Row::DimScale,
                label: "Dim the panes you are not in",
                // The ladder's *order* is not offered — it says what
                // marspot knows about each pane.  Only its volume is.
                cost: "deeper tells you at a glance what has drifted; \
                       shallower keeps it readable",
            },
            RowSpec {
            row: Row::CircledWide,
            label: "Circled digits take two cells",
            // The line this whole day bought.  Shipped as a default
            // once, reverted within the hour — so it is offered with
            // what it costs written next to it, and off.
            cost: "sized like CJK, but moves the wrap point: text can strand",
            },
        ],
    },
    Section {
        heading: "Scrolling",
        rows: &[RowSpec {
            row: Row::ScrollFactor,
            label: "Wheel speed",
            cost: "faster gets there in fewer flicks and overshoots in one",
        }],
    },
];

/// Index of the chosen value in a float choice list, or `usize::MAX`
/// when the file holds something the panel has no button for.
///
/// Compared with a tolerance rather than `==`: the value made the
/// round trip through a decimal string in the file, and a button that
/// stops lighting up because `0.6` came back as `0.60000002` would be
/// a bug nobody could see the cause of.
fn chosen_f32(choices: &[f32], have: f32) -> usize {
    choices
        .iter()
        .position(|c| (c - have).abs() < 1e-3)
        .unwrap_or(usize::MAX)
}

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
            Row::DimScale => Control::Segmented {
                options: DIM_LABELS,
                chosen: chosen_f32(DIM_CHOICES, s.dim_scale),
            },
            Row::ScrollFactor => Control::Segmented {
                options: SCROLL_LABELS,
                chosen: chosen_f32(SCROLL_CHOICES, s.scroll_factor),
            },
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
            Row::ReclaimEnabled
            | Row::CircledWide
            | Row::DimScale
            | Row::ScrollFactor => false,
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
            Row::DimScale => next.dim_scale = *DIM_CHOICES.get(seg)?,
            Row::ScrollFactor => next.scroll_factor = *SCROLL_CHOICES.get(seg)?,
        }
        (next != *s).then_some(next)
    }
}

/// Every row, flattened in draw order.
pub fn rows() -> impl Iterator<Item = &'static RowSpec> {
    SECTIONS.iter().flat_map(|s| s.rows.iter())
}

/// A piece of the panel, with where it goes.  The walker emits these
/// in draw order; the painter draws them and the hit-test looks at
/// them, so a control cannot be clickable anywhere but where it is
/// drawn.
pub enum Slot {
    Title { baseline: f64 },
    Group { heading: &'static str, baseline: f64 },
    /// A group's card.  Emitted before the rows it contains.
    Card { rect: Rect },
    Row {
        row: Row,
        spec: &'static RowSpec,
        /// The row's full band inside the card — what a hover would
        /// highlight, and what the hairline sits at the bottom of.
        band: Rect,
        label_baseline: f64,
        desc_baseline: f64,
        /// Right-aligned in the card, centred on the title line.
        control: Rect,
    },
    /// Hairline between two rows of the same card.
    Separator { rect: Rect },
    Footer { baseline: f64 },
}

use marspot_term::layout::Rect;

/// Measures a string in the UI font: `(text, pt, weight) -> width`.
///
/// Threaded in rather than approximated from a character count: the
/// panel sizes segments to their labels, and a count-based guess is
/// how the label came to overrun the button drawn to hold it.
pub type Measure<'a> = &'a mut dyn FnMut(&str, f64, u16) -> f64;

/// One row's height — constant across a card whatever control it
/// holds, so the card does not step.
fn row_h() -> f64 {
    metric::ROW_PAD_Y * 2.0
        + metric::ROW_BAND_H
        + metric::DESC_GAP
        + metric::DESC.cap()
        + metric::DESC.descent()
}

/// Walk the panel in draw order.  **One walker**, shared by the
/// painter, the hit-test and the height calculation — the alternative
/// is a layout pass plus a matching set of coordinates in the click
/// handler, which is how a control ends up reacting a row above where
/// it is drawn with nothing in a screenshot to say which is wrong.
///
/// `rect` is the panel in physical px; everything below is computed in
/// pt and scaled on the way out.
pub fn walk(rect: Rect, s: &Settings, m: Measure<'_>, mut on: impl FnMut(Slot)) {
    let px = crate::ui::core::ViewPainter::px_per_pt();
    let card_x = rect.x + metric::PAD_X * px;
    let card_w = rect.w - 2.0 * metric::PAD_X * px;
    let text_x = card_x + metric::CARD_PAD_X * px;

    // `y` walks in pt from the panel's top edge.
    let mut y = metric::PAD_TOP;
    let title_baseline = y + metric::TITLE.cap();
    on(Slot::Title { baseline: rect.y_top + title_baseline * px });
    y = title_baseline;

    for (si, section) in SECTIONS.iter().enumerate() {
        y += if si == 0 { metric::TITLE_TO_GROUP } else { metric::CARD_TO_GROUP };
        on(Slot::Group {
            heading: section.heading,
            baseline: rect.y_top + y * px,
        });
        y += metric::GROUP_TO_CARD;

        let card_top = y;
        let card_h = row_h() * section.rows.len() as f64;
        on(Slot::Card {
            rect: Rect {
                x: card_x,
                y_top: rect.y_top + card_top * px,
                w: card_w,
                h: card_h * px,
            },
        });

        for (ri, spec) in section.rows.iter().enumerate() {
            let row_top = card_top + row_h() * ri as f64;
            let band_top = row_top + metric::ROW_PAD_Y;
            // Label optically centred in the band, so a 13pt label and
            // a 22pt control share one centre line.
            let label_baseline = band_top + (metric::ROW_BAND_H + metric::LABEL.cap()) * 0.5;
            let desc_baseline =
                band_top + metric::ROW_BAND_H + metric::DESC_GAP + metric::DESC.cap();
            let ctl_h = match spec.row.control(s) {
                Control::Toggle(_) => metric::TOGGLE_H,
                Control::Segmented { .. } => metric::SEG_H,
            };
            let ctl_w = control_width(spec.row, s, m) / px;
            on(Slot::Row {
                row: spec.row,
                spec,
                band: Rect {
                    x: card_x,
                    y_top: rect.y_top + row_top * px,
                    w: card_w,
                    h: row_h() * px,
                },
                label_baseline: rect.y_top + label_baseline * px,
                desc_baseline: rect.y_top + desc_baseline * px,
                control: Rect {
                    x: card_x + card_w - (metric::CARD_PAD_X + ctl_w) * px,
                    y_top: rect.y_top + (band_top + (metric::ROW_BAND_H - ctl_h) * 0.5) * px,
                    w: ctl_w * px,
                    h: ctl_h * px,
                },
            });
            if ri + 1 < section.rows.len() {
                on(Slot::Separator {
                    rect: Rect {
                        x: text_x,
                        y_top: rect.y_top + (row_top + row_h()) * px,
                        w: card_x + card_w - text_x,
                        h: metric::SEP_H,
                    },
                });
            }
        }
        y = card_top + card_h;
    }

    y += metric::CARD_TO_FOOTER + metric::FOOTER.cap();
    on(Slot::Footer { baseline: rect.y_top + y * px });
}

/// The panel's height in pt, from the same walk that draws it — so
/// "the panel has a dead band at the bottom" is not expressible.
fn panel_h_pt(s: &Settings, m: Measure<'_>) -> f64 {
    // The walk needs a rect; height does not depend on it, so any
    // origin does.
    let probe = Rect { x: 0.0, y_top: 0.0, w: metric::PANEL_W, h: 0.0 };
    let px = crate::ui::core::ViewPainter::px_per_pt();
    let mut bottom = 0.0f64;
    walk(probe, s, m, |slot| {
        if let Slot::Footer { baseline } = slot {
            bottom = baseline / px + metric::FOOTER.descent() + metric::PAD_BOTTOM;
        }
    });
    bottom
}

/// The panel's rect, centred, **sized by what it actually draws**.
pub fn panel_rect(
    w_phys: f64,
    h_phys: f64,
    s: &Settings,
    m: Measure<'_>,
    top_inset: f64,
) -> Rect {
    let px = crate::ui::core::ViewPainter::px_per_pt();
    let w = (metric::PANEL_W * px).min(w_phys * 0.94);
    let h = (panel_h_pt(s, m) * px).min(h_phys * 0.94);
    Rect {
        x: (w_phys - w) / 2.0,
        y_top: ((h_phys - h) / 2.0).max(top_inset + 8.0),
        w,
        h,
    }
}

/// How wide this row's control needs to be, in physical px.
pub fn control_width(row: Row, s: &Settings, m: Measure<'_>) -> f64 {
    let px = crate::ui::core::ViewPainter::px_per_pt();
    match row.control(s) {
        Control::Toggle(_) => metric::TOGGLE_W * px,
        Control::Segmented { options, .. } => {
            let gaps = metric::SEG_GAP * px * options.len().saturating_sub(1) as f64;
            options.iter().map(|l| segment_width(l, m)).sum::<f64>() + gaps
        }
    }
}

/// A segment, sized to its own measured label.
fn segment_width(label: &str, m: Measure<'_>) -> f64 {
    let px = crate::ui::core::ViewPainter::px_per_pt();
    m(label, metric::SEGMENT.pt(), metric::SEGMENT.weight()) + 2.0 * metric::SEG_PAD_X * px
}

/// The `n`th segment of a segmented control.
pub fn segment_rect(control: Rect, n: usize, labels: &[&str], m: Measure<'_>) -> Rect {
    let px = crate::ui::core::ViewPainter::px_per_pt();
    let gap = metric::SEG_GAP * px;
    let mut x = control.x;
    for l in labels.iter().take(n) {
        x += segment_width(l, m) + gap;
    }
    Rect {
        x,
        y_top: control.y_top,
        w: labels.get(n).map(|l| segment_width(l, m)).unwrap_or(0.0),
        h: control.h,
    }
}

/// Left edge for a row's text.
pub fn text_x(rect: Rect) -> f64 {
    let px = crate::ui::core::ViewPainter::px_per_pt();
    rect.x + (metric::PAD_X + metric::CARD_PAD_X) * px
}

/// Which row and segment `(px_x, py)` lands on, if any.
pub fn hit_test(
    rect: Rect,
    s: &Settings,
    m: Measure<'_>,
    px_x: f64,
    py: f64,
) -> Option<(Row, usize)> {
    // Collect first: the walker borrows `m` for the duration, and the
    // segment geometry needs it again.
    let mut controls: Vec<(Row, Rect)> = Vec::new();
    walk(rect, s, m, |slot| {
        if let Slot::Row { row, control, .. } = slot {
            controls.push((row, control));
        }
    });
    for (row, control) in controls {
        match row.control(s) {
            Control::Toggle(_) => {
                if control.contains(px_x, py) {
                    return Some((row, 0));
                }
            }
            Control::Segmented { options, .. } => {
                for n in 0..options.len() {
                    if segment_rect(control, n, options, m).contains(px_x, py) {
                        return Some((row, n));
                    }
                }
            }
        }
    }
    None
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

    /// Stand-in for the font: SF Pro's average advance is close
    /// enough to 0.5 em for a geometry test, and the point of these
    /// assertions is that widths *come from a measurement* at all.
    fn fake_measure() -> impl FnMut(&str, f64, u16) -> f64 {
        |s: &str, pt: f64, w: u16| {
            let bold = if w >= 600 { 1.05 } else { 1.0 };
            s.chars().count() as f64 * pt * 0.5 * bold
                * crate::ui::core::ViewPainter::px_per_pt()
        }
    }

    fn collect(rect: Rect, s: &Settings) -> Vec<Slot> {
        let mut m = fake_measure();
        let mut out = Vec::new();
        walk(rect, s, &mut m, |slot| out.push(slot));
        out
    }

    fn rows_of(slots: &[Slot]) -> Vec<&Slot> {
        slots.iter().filter(|s| matches!(s, Slot::Row { .. })).collect()
    }

    fn test_rect(w: f64, h: f64, s: &Settings) -> Rect {
        let mut m = fake_measure();
        panel_rect(w, h, s, &mut m, 30.0)
    }

    /// The claim `walk` is built on: a control is clickable exactly
    /// where it is drawn.  Aim at the middle of each control the
    /// painter would paint, and the hit-test must name that same row.
    #[test]
    fn every_control_is_clickable_where_it_is_drawn() {
        let s = Settings::default();
        let rect = test_rect(1600.0, 1200.0, &s);
        let slots = collect(rect, &s);
        let mut m = fake_measure();

        assert_eq!(rows_of(&slots).len(), rows().count(), "every row is walked once");

        for slot in &slots {
            let Slot::Row { row, control, .. } = slot else { continue };
            match row.control(&s) {
                Control::Toggle(_) => {
                    let hit = hit_test(
                        rect, &s, &mut m,
                        control.x + control.w / 2.0,
                        control.y_top + control.h / 2.0,
                    );
                    assert_eq!(hit, Some((*row, 0)), "toggle {row:?}");
                }
                Control::Segmented { options, .. } => {
                    for n in 0..options.len() {
                        let sr = segment_rect(*control, n, options, &mut m);
                        let hit = hit_test(
                            rect, &s, &mut m,
                            sr.x + sr.w / 2.0,
                            sr.y_top + sr.h / 2.0,
                        );
                        assert_eq!(hit, Some((*row, n)), "segment {n} of {row:?}");
                    }
                }
            }
        }
        // A click in the panel's empty space hits nothing.
        assert_eq!(hit_test(rect, &s, &mut m, rect.x + 2.0, rect.y_top + 2.0), None);
    }

    /// What the panel was rebuilt for, as assertions: rows must read
    /// as separate items inside a card, and nothing may overflow.
    #[test]
    fn rows_are_separated_and_nothing_overflows() {
        let s = Settings::default();
        let rect = test_rect(1800.0, 1400.0, &s);
        let slots = collect(rect, &s);
        let mut m = fake_measure();
        let px = crate::ui::core::ViewPainter::px_per_pt();

        // Every row sits inside the card that was announced for it, and
        // every card inside the panel.
        let mut card: Option<Rect> = None;
        let mut rows_in_card = 0usize;
        for slot in &slots {
            match slot {
                Slot::Card { rect: c } => {
                    assert!(
                        c.x >= rect.x && c.x + c.w <= rect.x + rect.w + 1e-9,
                        "card escapes the panel"
                    );
                    card = Some(*c);
                    rows_in_card = 0;
                }
                Slot::Row { row, band, label_baseline, desc_baseline, control, .. } => {
                    let c = card.expect("a row before any card");
                    rows_in_card += 1;
                    assert!(
                        band.y_top >= c.y_top - 1e-9
                            && band.y_top + band.h <= c.y_top + c.h + 1e-9,
                        "{row:?} band escapes its card"
                    );
                    // Two-tier text: the cost line is under the label,
                    // and it is *smaller*, not merely dimmer.
                    assert!(desc_baseline > label_baseline, "{row:?} cost line above its label");
                    assert!(metric::DESC.pt() < metric::LABEL.pt(), "no type scale");
                    // The control clears the cost line under it — the
                    // reason the label line is a band, not a baseline.
                    assert!(
                        control.y_top + control.h
                            <= desc_baseline
                                - metric::DESC.cap()
                                    * px
                                + 1e-9,
                        "{row:?} control overlaps its own cost line"
                    );
                    // Right-aligned on one column, inside the card.
                    let want = c.x + c.w - metric::CARD_PAD_X * px;
                    assert!(
                        (control.x + control.w - want).abs() < 0.5,
                        "{row:?} control off the column"
                    );
                    if let Control::Segmented { options, .. } = row.control(&s) {
                        for (n, label) in options.iter().enumerate() {
                            let sr = segment_rect(*control, n, options, &mut m);
                            let ink = fake_measure()(label, metric::SEGMENT.pt(), metric::SEGMENT.weight());
                            assert!(sr.w > ink, "{label:?} needs {ink:.1}, button is {:.1}", sr.w);
                        }
                        let last =
                            segment_rect(*control, options.len() - 1, options, &mut m);
                        assert!(
                            last.x + last.w <= control.x + control.w + 0.5,
                            "the last segment runs past the control"
                        );
                    }
                }
                Slot::Separator { rect: sep } => {
                    let c = card.expect("a separator before any card");
                    assert!(
                        sep.x > c.x,
                        "the hairline must be inset from the card's left edge"
                    );
                    assert!(rows_in_card >= 1, "a hairline before the first row");
                }
                _ => {}
            }
        }

        // A single-row card gets no hairline; a three-row card gets two.
        let seps = slots.iter().filter(|s| matches!(s, Slot::Separator { .. })).count();
        let expect: usize = SECTIONS.iter().map(|s| s.rows.len() - 1).sum();
        assert_eq!(seps, expect, "one hairline between each pair of rows, none at the edges");

        // No dead band: the footer sits just above the bottom edge.
        let foot = slots
            .iter()
            .find_map(|s| match s {
                Slot::Footer { baseline } => Some(*baseline),
                _ => None,
            })
            .expect("footer");
        let bottom = rect.y_top + rect.h;
        assert!(foot < bottom, "footer outside the panel");
        assert!(
            bottom - foot < (metric::PAD_BOTTOM + metric::FOOTER.pt()) * px,
            "dead space under the footer: {:.1}px",
            bottom - foot
        );
    }

    /// The panel is roomy on purpose — one of the two complaints that
    /// started this rewrite was that everything was crammed together.
    ///
    /// Stated **relative to the type**, not in absolute points: the
    /// first cut asserted "a row is at least 44pt", which fired the
    /// moment the type came down a rung even though the proportions
    /// were unchanged.  A test that has to be edited whenever the
    /// scale moves is not guarding the thing it claims to guard.
    #[test]
    fn a_row_is_mostly_space_not_text() {
        let s = Settings::default();
        let rect = test_rect(2400.0, 1800.0, &s);
        let px = crate::ui::core::ViewPainter::px_per_pt();
        let slots = collect(rect, &s);
        let rows: Vec<_> = rows_of(&slots)
            .iter()
            .map(|s| match s {
                Slot::Row { band, .. } => *band,
                _ => unreachable!(),
            })
            .collect();
        for w in rows.windows(2) {
            let gap = w[1].y_top - (w[0].y_top + w[0].h);
            assert!(gap >= -1e-9, "rows overlap");
        }
        // Ink versus room: the two lines of a row must not fill it.
        let ink = metric::LABEL.cap() + metric::DESC_GAP + metric::DESC.cap();
        let row_h = rows[0].h / px;
        assert!(
            ink / row_h < 0.62,
            "a row is {:.0}% text — that is the cramped panel again",
            ink / row_h * 100.0,
        );
        // And the text column holds a real sentence at the label size,
        // whatever that size currently is.
        let column = (rect.w / px) - 2.0 * (metric::PAD_X + metric::CARD_PAD_X);
        assert!(
            column > 30.0 * metric::LABEL.cap(),
            "text column {column:.0}pt is narrow for {:.1}pt type",
            metric::LABEL.pt(),
        );
    }

    /// The same geometry, measured with the **real font**.
    ///
    /// `fake_measure` proves the layout is self-consistent; it cannot
    /// prove SF Pro fits, and "the button is narrower than the label
    /// inside it" is precisely a real-metrics failure.  Skipped rather
    /// than failed if the font stack will not build.
    #[test]
    fn the_real_font_fits_the_boxes_drawn_for_it() {
        let Ok(mut font) = crate::font_cache::FontCache::build() else {
            eprintln!("no font stack; skipping");
            return;
        };
        let px = crate::ui::core::ViewPainter::px_per_pt();
        let s = Settings::default();
        let mut m = |t: &str, pt: f64, w: u16| {
            font.measure_ui_text_at_size(
                t, w, crate::font_shape::ShapeOptions::default(), pt,
            )
        };
        let rect = panel_rect(2000.0, 1500.0, &s, &mut m, 30.0);

        let mut slots = Vec::new();
        walk(rect, &s, &mut m, |slot| slots.push(slot));

        let mut card = Rect { x: 0.0, y_top: 0.0, w: 0.0, h: 0.0 };
        for slot in &slots {
            match slot {
                Slot::Card { rect: c } => card = *c,
                Slot::Row { row, spec, control, .. } => {
                    let text_left = card.x + metric::CARD_PAD_X * px;
                    // The cost line is the long text in this panel: it
                    // must fit the card, or it runs out of the box.
                    let cost_w = m(spec.cost, metric::DESC.pt(), metric::DESC.weight());
                    assert!(
                        text_left + cost_w <= card.x + card.w - metric::CARD_PAD_X * px,
                        "{:?}: cost line needs {:.0}pt and the card gives {:.0}pt",
                        row,
                        cost_w / px,
                        (card.w - 2.0 * metric::CARD_PAD_X * px) / px,
                    );
                    // The label and the control share one line and must
                    // not collide.
                    let label_w = m(spec.label, metric::LABEL.pt(), metric::LABEL.weight());
                    assert!(
                        text_left + label_w + 12.0 * px <= control.x,
                        "{:?}: label runs into its control",
                        row
                    );
                    if let Control::Segmented { options, .. } = row.control(&s) {
                        for (n, label) in options.iter().enumerate() {
                            let seg = segment_rect(*control, n, options, &mut m);
                            let ink = m(label, metric::SEGMENT.pt(), metric::SEGMENT.weight());
                            assert!(
                                seg.w >= ink + 2.0 * metric::SEG_PAD_X * px - 0.5,
                                "{label:?} ink {:.0}pt vs button {:.0}pt",
                                ink / px,
                                seg.w / px,
                            );
                        }
                    }
                }
                _ => {}
            }
        }
        // And the panel it all sits in is the size it claims.
        assert!(rect.w > 0.0 && rect.h > 0.0);
    }

    /// Every geometric promise this panel makes, at **both** densities.
    ///
    /// The panels were only ever laid out and eyeballed on a display
    /// whose backing scale is 1; every scale bug this session had —
    /// the toolbar on the traffic lights, the cards outside their
    /// modal, boxes doubling while their text stood still — was a
    /// second density nobody had rendered.  So the invariants run at
    /// both, and `chrome_scale` is a process global that nextest's
    /// per-test process isolation makes safe to set here.
    #[test]
    fn the_panel_holds_together_at_every_density() {
        for scale in [1.0f64, 2.0] {
            crate::ui::set_chrome_scale(scale);
            let px = crate::ui::core::ViewPainter::px_per_pt();
            let s = Settings::default();
            let rect = test_rect(2400.0 * scale, 1800.0 * scale, &s);
            let slots = collect(rect, &s);
            let mut m = fake_measure();

            // The panel scales with the density rather than staying a
            // fixed pixel count — a panel that did not would be half
            // its intended size on a retina Mac.
            assert!(
                (rect.w - metric::PANEL_W * px).abs() < 1.0,
                "scale {scale}: panel {:.0}px is not {:.0}pt",
                rect.w, metric::PANEL_W,
            );

            let mut card: Option<Rect> = None;
            let mut rows = 0usize;
            for slot in &slots {
                match slot {
                    Slot::Card { rect: c } => {
                        assert!(
                            c.x >= rect.x - 1e-6
                                && c.x + c.w <= rect.x + rect.w + 1e-6,
                            "scale {scale}: card escapes the panel",
                        );
                        card = Some(*c);
                    }
                    Slot::Row { row, band, control, desc_baseline, .. } => {
                        let c = card.expect("row before card");
                        rows += 1;
                        assert!(
                            band.y_top >= c.y_top - 1e-6
                                && band.y_top + band.h <= c.y_top + c.h + 1e-6,
                            "scale {scale}: {row:?} band escapes its card",
                        );
                        assert!(
                            control.x + control.w <= c.x + c.w + 1e-6,
                            "scale {scale}: {row:?} control escapes its card",
                        );
                        assert!(
                            control.y_top + control.h <= *desc_baseline + 1e-6,
                            "scale {scale}: {row:?} control overlaps its cost line",
                        );
                        if let Control::Segmented { options, .. } = row.control(&s) {
                            let last = segment_rect(
                                *control, options.len() - 1, options, &mut m,
                            );
                            assert!(
                                last.x + last.w <= control.x + control.w + 0.5,
                                "scale {scale}: the last segment runs past the control",
                            );
                        }
                    }
                    Slot::Footer { baseline } => assert!(
                        *baseline < rect.y_top + rect.h,
                        "scale {scale}: the footer fell out of the panel",
                    ),
                    _ => {}
                }
            }
            assert_eq!(rows, super::rows().count(), "scale {scale}: rows lost");
        }
        crate::ui::set_chrome_scale(1.0);
    }

    #[test]
    fn the_panel_fits_the_window_it_is_centred_in() {
        let s = Settings::default();
        for (w, h) in [(1200.0, 800.0), (400.0, 300.0), (3840.0, 2160.0)] {
            let r = panel_rect(w, h, &s, &mut fake_measure(), 30.0);
            assert!(r.w <= w, "wider than the window at {w}x{h}");
            assert!(r.h <= h, "taller than the window at {w}x{h}");
            assert!(r.x >= 0.0 && r.y_top >= 0.0, "off-screen at {w}x{h}");
        }
    }
}
