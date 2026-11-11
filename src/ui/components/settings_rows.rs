//! The settings panel's rows as data: which setting each edits, the
//! control it shows for the settings in force, and what a click on
//! that control changes.

use crate::settings::Settings;
use crate::ui::components::settings_choices::*;
use crate::ui::strings::Msg;

/// Which setting a row edits.  One variant per row — the panel has no
/// generic "key" plumbing on purpose: a typo'd key would compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    ReclaimEnabled,
    ReclaimIdleMinutes,
    ReclaimPrefetch,
    Theme,
    Language,
    DimScale,
    CircledWide,
    ScrollFactor,
    CcStatuslineHook,
}

/// A row's control shape.
pub enum Control {
    /// On / off.
    Toggle(bool),
    /// One of N, with the chosen index.
    Segmented { options: &'static [Msg], chosen: usize },
}

pub struct RowSpec {
    pub row: Row,
    /// The label, as a catalogue id.  A `const` table cannot call a lookup
    /// that depends on the running locale, so the id travels and the view
    /// resolves it where it draws (`crate::ui::strings`).
    pub label: Msg,
    /// What it costs.  Present on every row; see the module note.
    pub cost: Msg,
}

pub struct Section {
    pub heading: Msg,
    pub rows: &'static [RowSpec],
}

/// English, like every other panel in the app.  Group headings are
/// sentence case, not shouted: they sit above their card at 12pt
/// semibold, where all-caps only adds noise.
pub const SECTIONS: &[Section] = &[
    Section {
        heading: Msg::SettingsIdleReclamation,
        rows: &[
            RowSpec {
                row: Row::ReclaimEnabled,
                label: Msg::ReclaimIdlePanes,
                cost: Msg::CostReclaimIdlePanes,
            },
            RowSpec {
                row: Row::ReclaimIdleMinutes,
                label: Msg::IdleFor,
                cost: Msg::CostIdleFor,
            },
            RowSpec {
                row: Row::ReclaimPrefetch,
                label: Msg::WarmUpOnReturn,
                cost: Msg::CostWarmUpOnReturn,
            },
        ],
    },
    Section {
        heading: Msg::SettingsAppearance,
        rows: &[
            RowSpec { row: Row::Theme, label: Msg::Theme, cost: Msg::CostTheme },
            RowSpec { row: Row::Language, label: Msg::Language, cost: Msg::CostLanguage },
            // The ladder's *order* is not offered — it says what
            // marspot knows about each pane.  Only its volume is.
            RowSpec { row: Row::DimScale, label: Msg::DimOtherPanes, cost: Msg::CostDimOtherPanes },
            // Shipped as a default once, reverted within the hour — so
            // it is offered with what it costs written next to it, off.
            RowSpec {
                row: Row::CircledWide,
                label: Msg::CircledDigitsWide,
                cost: Msg::CostCircledDigitsWide,
            },
        ],
    },
    Section {
        heading: Msg::SettingsClaudeCode,
        rows: &[RowSpec {
            row: Row::CcStatuslineHook,
            label: Msg::ClaudeCodeReportsModel,
            // Says whose file it is, because that is the whole cost.
            // Off, the badge still names the model — from the session
            // transcript and the pane's own banner — it is just a turn
            // behind where those go quiet.
            cost: Msg::CostClaudeCodeReportsModel,
        }],
    },
    Section {
        heading: Msg::SettingsScrolling,
        rows: &[RowSpec {
            row: Row::ScrollFactor,
            label: Msg::WheelSpeed,
            cost: Msg::CostWheelSpeed,
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
            Row::CcStatuslineHook => Control::Toggle(s.cc_statusline_hook),
            Row::CircledWide => Control::Toggle(s.appearance_circled_wide),
            Row::Theme => Control::Segmented {
                options: THEME_LABELS,
                chosen: THEME_CHOICES.iter().position(|t| *t == s.appearance_theme).unwrap_or(usize::MAX),
            },
            Row::Language => Control::Segmented {
                options: LANGUAGE_LABELS,
                chosen: LANGUAGE_CHOICES.iter().position(|l| *l == s.appearance_language).unwrap_or(usize::MAX),
            },
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
            | Row::Theme
            | Row::Language
            | Row::CircledWide
            | Row::DimScale
            | Row::ScrollFactor
            | Row::CcStatuslineHook => false,
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
            Row::CcStatuslineHook => next.cc_statusline_hook = !s.cc_statusline_hook,
            Row::CircledWide => next.appearance_circled_wide = !s.appearance_circled_wide,
            Row::ReclaimIdleMinutes => {
                next.reclaim_idle_minutes = *IDLE_CHOICES.get(seg)?;
            }
            Row::Theme => next.appearance_theme = *THEME_CHOICES.get(seg)?,
            Row::Language => next.appearance_language = *LANGUAGE_CHOICES.get(seg)?,
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
