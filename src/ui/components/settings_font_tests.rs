//! The font rows of the settings panel: the family steps through the
//! monospaced families this Mac has, the size through whole points, and
//! a click writes both into the file.

use super::settings_rows::{Control, Row};
use crate::settings::{self, Settings};

#[test]
fn the_family_row_steps_through_the_installed_monospaced_families() {
    crate::font_families::refresh_now();
    let list = crate::font_families::installed();
    assert!(list.len() > 1, "macOS ships Menlo and Monaco: {list:?}");
    let s = Settings::default();
    let at = list.iter().position(|f| *f == s.font_family).expect("Monaco is listed");
    let next = Row::FontFamily.apply(&s, 1).expect("a next family");
    assert_eq!(next.font_family, list[(at + 1) % list.len()]);
    let back = Row::FontFamily.apply(&next, 0).expect("and back");
    assert_eq!(back.font_family, s.font_family);
    match Row::FontFamily.control(&s) {
        Control::Picker { value, arrows, prev, next } => {
            assert_eq!(value, "Monaco");
            assert!(arrows && prev && next, "wraps, so both buttons work");
        }
        _ => panic!("wrong control"),
    }
    // what a click writes is what the file reads back
    let written = settings::render(&next, "");
    assert_eq!(settings::parse(&written).font_family, next.font_family, "{written}");
}

#[test]
fn the_size_row_steps_whole_points_inside_its_range() {
    let at = |pt: f32| Settings { font_size: pt, ..Settings::default() };
    let size = |s: Option<Settings>| s.map(|s| s.font_size);
    assert_eq!(size(Row::FontSize.apply(&at(12.0), 1)), Some(13.0));
    assert_eq!(size(Row::FontSize.apply(&at(12.0), 0)), Some(11.0));
    // a hand-edited half point steps to the whole point on that side
    assert_eq!(size(Row::FontSize.apply(&at(13.5), 0)), Some(13.0));
    assert_eq!(size(Row::FontSize.apply(&at(13.5), 1)), Some(14.0));
    // the ends of the range, and a hand edit beyond them steps into it
    assert_eq!(size(Row::FontSize.apply(&at(24.0), 1)), None);
    assert_eq!(size(Row::FontSize.apply(&at(9.0), 0)), None);
    assert_eq!(size(Row::FontSize.apply(&at(30.0), 0)), Some(24.0));
    assert_eq!(size(Row::FontSize.apply(&at(8.0), 1)), Some(9.0));
    match Row::FontSize.control(&at(24.0)) {
        Control::Picker { value, arrows, prev, next } => {
            assert_eq!(value, "24");
            assert!(!arrows && prev && !next);
        }
        _ => panic!("wrong control"),
    }
    match Row::FontSize.control(&at(13.5)) {
        Control::Picker { value, .. } => assert_eq!(value, "13.5"),
        _ => panic!("wrong control"),
    }
}

/// Where the panel goes is worked out every frame it is open, and the
/// font rows must not make that allocate: their sizes do not depend on
/// the family's name or the size's digits.
#[test]
fn placing_the_panel_with_the_font_rows_allocates_nothing() {
    use crate::ui::components::{controls, settings_modal};
    crate::font_families::refresh_now();
    let mut font = crate::font_cache::FontCache::build().expect("font");
    let s = Settings { font_family: "Menlo".into(), font_size: 13.5, ..Settings::default() };
    let place = |font: &mut crate::font_cache::FontCache| {
        let mut m = controls::font_measure(font);
        settings_modal::panel_rect(1000.0, 1200.0, &s, &mut m, 0.0)
    };
    let warm = place(&mut font);
    let (n, again) = crate::frame_alloc_tests::counted(|| place(&mut font));
    assert_eq!(n, 0, "placing the panel allocated {n} times");
    assert_eq!(again, warm);
}
