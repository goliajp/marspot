//! The settings panel scrolled to its Claude Code section, with the
//! switching profile set to full use: a picture at each display scale
//! and in each language (`settings_sched{,.2x}{,.ja,.zh}.png`).

use super::controls;
use super::density_kit::at_scale;
use super::panel_snapshot_tests::snapshot;
use super::settings_modal;
use super::settings_view::SettingsRender;
use crate::settings::{SchedProfile, Settings};

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
