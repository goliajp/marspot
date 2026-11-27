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

/// At 2x the cell is exactly twice the 1x cell, and a family chosen
/// and then put back at 2x leaves the cell where it was.
#[test]
fn the_cell_doubles_at_2x_and_a_family_round_trip_restores_it() {
    settings::set_for_test(Settings::default());
    let saved = crate::ui::chrome_scale();
    crate::ui::set_chrome_scale(1.0);
    let mut r = match MetalRenderer::new_headless() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            return;
        }
    };
    r.rebuild_fonts_if_stale();
    let (w1, h1) = r.cell_dims();

    crate::ui::set_chrome_scale(2.0);
    assert!(r.rebuild_fonts_if_stale(), "a new density rebuilds");
    let (w2, h2) = r.cell_dims();
    assert!(
        (w2 - 2.0 * w1).abs() < 1e-9 && (h2 - 2.0 * h1).abs() < 1e-9,
        "{w1}x{h1} at 1x -> {w2}x{h2} at 2x",
    );

    settings::set_for_test(Settings { font_family: "Menlo".into(), ..Settings::default() });
    assert!(r.rebuild_fonts_if_stale());
    assert_eq!(r.font_mut().font(0).expect("the base font").family_name(), "Menlo");
    settings::set_for_test(Settings::default());
    assert!(r.rebuild_fonts_if_stale());
    assert_eq!(r.font_mut().font(0).expect("the base font").family_name(), "Monaco");
    assert_eq!(r.cell_dims(), (w2, h2), "back to Monaco at 2x");

    crate::ui::set_chrome_scale(saved);
    r.rebuild_fonts_if_stale();
}
