//! What each segmented row of the settings panel offers: the values,
//! and the label of each, in the same order.

use marspot_term::language::LangId;
use marspot_term::palette::ThemeId;

use crate::ui::strings::Msg;

/// The minutes offered by the idle-threshold row, in order.
///
/// `0` is "never", offered as a value and not only as the switch above
/// it: the end of the range is where a user turning it down looks.
pub const IDLE_CHOICES: &[u32] = &[15, 30, 60, 120, 0];
pub const IDLE_LABELS: &[Msg] = &[Msg::Idle15m, Msg::Idle30m, Msg::Idle1h, Msg::Idle2h, Msg::IdleNever];

/// How far an inactive pane steps back, as a multiplier on the
/// attention ladder.  `0` is off, a value for the reason `Never` is.
pub const DIM_CHOICES: &[f32] = &[0.0, 0.6, 1.0, 1.4];
pub const DIM_LABELS: &[Msg] = &[Msg::DimOff, Msg::DimLight, Msg::DimNormal, Msg::DimDeep];

/// The themes, with following the system first: `None` in the setting.
pub const THEME_CHOICES: &[Option<ThemeId>] =
    &[None, Some(ThemeId::Dark), Some(ThemeId::Light), Some(ThemeId::Contrast)];
pub const THEME_LABELS: &[Msg] = &[Msg::ThemeSystem, Msg::ThemeDark, Msg::ThemeLight, Msg::ThemeContrast];

/// Wheel / trackpad multiplier.
pub const SCROLL_CHOICES: &[f32] = &[0.5, 1.0, 1.5, 2.5];
pub const SCROLL_LABELS: &[Msg] = &[Msg::ScrollSlow, Msg::ScrollNormal, Msg::ScrollFast, Msg::ScrollFaster];

/// The languages, with following the system first: `None` in the
/// setting.  Each language is named in itself.
pub const LANGUAGE_CHOICES: &[Option<LangId>] =
    &[None, Some(LangId::En), Some(LangId::Ja), Some(LangId::ZhHans)];
pub const LANGUAGE_LABELS: &[Msg] =
    &[Msg::ThemeSystem, Msg::LangEnglish, Msg::LangJapanese, Msg::LangChinese];

/// The line under the language row: what choosing costs, or, while the
/// items the system provides are still in the language Marspot was
/// launched in, that they follow at the next launch.
pub fn language_cost() -> Msg {
    match marspot_term::system_language::launched_in() {
        Some(launched) if launched != marspot_term::language::current_id() => Msg::CostLanguageRelaunch,
        _ => Msg::CostLanguage,
    }
}
