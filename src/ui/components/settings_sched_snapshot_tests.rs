//! The settings panel scrolled to its Claude Code section, with the
//! switching profile set to full use: a picture at each display scale
//! and in each language (`settings_sched{,.2x}{,.ja,.zh}.png`).

use super::controls;
use super::density_kit::at_scale;
use super::panel_snapshot_tests::snapshot;
use super::settings_modal;
use super::settings_rows::{Control, Row};
use super::settings_view::SettingsRender;
use crate::settings::{SchedProfile, Settings};
use crate::ui::strings::{Msg, table};

/// How far down the panel the picture starts, before the scale.
const SCROLL: f64 = 760.0;

pub(super) fn panel_snapshot_settings_sched() {
    snapshot("settings_sched", 1000, 1200, |renderer, layout| {
        let mut settings = Settings {
            appearance_theme: crate::settings::get().appearance_theme,
            appearance_language: crate::settings::get().appearance_language,
            ..Default::default()
        };
        settings.sched.profile = SchedProfile::Full;
        let mut measure = controls::font_measure(renderer.font_mut());
        let rect = settings_modal::panel_rect(layout.window_w, layout.window_h, &settings, &mut measure, layout.top_inset);
        let scroll = settings_modal::max_scroll(rect, &settings, &mut measure).min(SCROLL * layout.window_w / 1000.0);
        assert!(scroll > 0.0, "the panel is taller than the window it is drawn in");
        drop(measure);
        let path = "/Users/you/Library/Application Support/marspot/settings.toml";
        renderer.set_settings_panel(Some(SettingsRender::new(rect, scroll, settings, path)));
    });
}

fn panel_snapshot_settings_sched_2x() {
    at_scale(2.0, panel_snapshot_settings_sched);
}

#[test]
fn panel_snapshot_settings_sched_en() {
    crate::snapshot_kit::in_language(marspot_term::language::LangId::En, panel_snapshot_settings_sched);
}

#[test]
fn panel_snapshot_settings_sched_en_2x() {
    crate::snapshot_kit::in_language(marspot_term::language::LangId::En, panel_snapshot_settings_sched_2x);
}

crate::snapshot_kit::snapshots_in_other_languages! {
    panel_snapshot_settings_sched_ja, panel_snapshot_settings_sched_zh = panel_snapshot_settings_sched;
    panel_snapshot_settings_sched_ja_2x, panel_snapshot_settings_sched_zh_2x = panel_snapshot_settings_sched_2x;
}

/// A settings file with no `sched.profile` line lights balanced, and the
/// row's three choices read 稳妥 / 平衡 / 充分利用; no language calls the
/// planner a trial or a dry run any more.
#[test]
fn settings_sched_row_defaults_to_balanced_and_names_the_three_profiles() {
    let s = crate::settings::parse("");
    assert_eq!(s.sched.profile, SchedProfile::Balanced);
    let Control::Segmented { options, chosen } = Row::SchedProfile.control(&s) else { panic!("the profile row is a segmented control") };
    let zh = table(marspot_term::language::LangId::ZhHans);
    let words: Vec<&str> = options.iter().map(|m| (zh.text)(*m)).collect();
    assert_eq!(words, ["稳妥", "平衡", "充分利用"]);
    assert_eq!(chosen, 1);
    for id in [marspot_term::language::LangId::En, marspot_term::language::LangId::Ja, marspot_term::language::LangId::ZhHans] {
        let cost = (table(id).text)(Msg::CostClaudeCodeSchedProfile).to_lowercase();
        for word in ["试运行", "試行", "trial", "dry run", "dry-run"] {
            assert!(!cost.contains(word), "{id:?}: {cost}");
        }
    }
}
