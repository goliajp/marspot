//! A font chosen in the settings reaches the grid without a restart:
//! the renderer rebuilds its fonts for it, and only when it changed.

use super::MetalRenderer;
use crate::settings::{self, Settings};

#[test]
fn a_font_chosen_in_the_settings_rebuilds_the_cell_once() {
    settings::set_for_test(Settings::default());
    let mut r = match MetalRenderer::new_headless() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            return;
        }
    };
    let (w0, h0) = r.cell_dims();
    assert!(!r.rebuild_fonts_if_stale(), "nothing moved");

    settings::set_for_test(Settings { font_size: 24.0, ..Settings::default() });
    assert!(r.rebuild_fonts_if_stale(), "a new size rebuilds");
    let (w1, h1) = r.cell_dims();
    assert!((w1 - w0 * 2.0).abs() < 0.5 && (h1 - h0 * 2.0).abs() < 1.0, "{w0}x{h0} -> {w1}x{h1}");
    assert!(!r.rebuild_fonts_if_stale(), "and settles");

    settings::set_for_test(Settings { font_family: "Menlo".into(), ..Settings::default() });
    assert!(r.rebuild_fonts_if_stale(), "a new family rebuilds");
    assert_eq!(r.font_mut().font(0).expect("the base font").family_name(), "Menlo");

    // a family this Mac lacks is drawn in Monaco, and is not taken for
    // a change on every look
    settings::set_for_test(Settings { font_family: "No Such Family 7f3a".into(), ..Settings::default() });
    assert!(r.rebuild_fonts_if_stale());
    assert_eq!(r.font_mut().font(0).expect("the base font").family_name(), "Monaco");
    assert_eq!(r.cell_dims(), (w0, h0));
    assert!(!r.rebuild_fonts_if_stale());

    settings::set_for_test(Settings::default());
    assert!(r.rebuild_fonts_if_stale());
    assert_eq!(r.cell_dims(), (w0, h0), "and comes back exactly");
}
