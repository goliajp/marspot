//! The settings panel scrolled to its last section, the agent CLIs,
//! with one CLI answered yes and one no: a picture at each display
//! scale (`settings_agents.png`, `settings_agents.2x.png`).

use super::controls;
use super::density_kit::at_scale;
use super::panel_snapshot_tests::snapshot;
use super::settings_modal;
use super::settings_view::SettingsRender;
use crate::settings::{InstallConsent, Settings};

#[test]
pub(super) fn panel_snapshot_settings_agents() {
    snapshot("settings_agents", 1000, 1552, |renderer, layout| {
        let mut settings = Settings {
            appearance_theme: crate::settings::get().appearance_theme,
            appearance_language: crate::settings::get().appearance_language,
            ..Default::default()
        };
        settings.install_consent.set("codex", InstallConsent::Yes);
        settings.install_consent.set("kimi", InstallConsent::No);
        let mut measure = controls::font_measure(renderer.font_mut());
        let rect = settings_modal::panel_rect(layout.window_w, layout.window_h, &settings, &mut measure, layout.top_inset);
        let bottom = settings_modal::max_scroll(rect, &settings, &mut measure);
        assert!(bottom > 0.0, "the panel is taller than the window it is drawn in");
        drop(measure);
        let path = "/Users/you/Library/Application Support/marspot/settings.toml";
        renderer.set_settings_panel(Some(SettingsRender::new(rect, bottom, settings, path)));
    });
}

#[test]
fn panel_snapshot_settings_agents_2x() {
    at_scale(2.0, panel_snapshot_settings_agents);
}
