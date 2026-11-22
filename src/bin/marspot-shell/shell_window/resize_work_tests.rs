use super::{resize_work, AttachedGeometry, ResizeWork};

fn geom(w: usize, h: usize, scale: f64, lights: f64) -> AttachedGeometry {
    AttachedGeometry { w_px: w, h_px: h, scale, lights_right_phys: lights }
}

/// The 2026-09-27 report: a window nobody was resizing flickered,
/// and the log had 144 resize notifications a second behind it,
/// each one allocating an IOSurface pair and swapping the
/// presenter onto a surface the core had only started painting.
#[test]
fn a_notification_with_nothing_new_costs_nothing() {
    let g = geom(2000, 1400, 2.0, 69.0);
    assert_eq!(resize_work(Some(g), &g), ResizeWork::Nothing);
}

#[test]
fn the_first_notification_always_attaches() {
    assert_eq!(resize_work(None, &geom(2000, 1400, 2.0, 69.0)), ResizeWork::Attach);
}

#[test]
fn moved_pixels_attach() {
    let prev = geom(2000, 1400, 2.0, 69.0);
    assert_eq!(resize_work(Some(prev), &geom(2000, 1401, 2.0, 69.0)), ResizeWork::Attach);
    assert_eq!(resize_work(Some(prev), &geom(1999, 1400, 2.0, 69.0)), ResizeWork::Attach);
}

/// A window dragged to a display of another density keeps its
/// pixel size and needs everything rebuilt anyway.
#[test]
fn a_new_density_attaches_at_the_same_size() {
    let prev = geom(2000, 1400, 2.0, 69.0);
    assert_eq!(resize_work(Some(prev), &geom(2000, 1400, 1.0, 69.0)), ResizeWork::Attach);
}

/// Full-screen moves the buttons without moving a pixel of the
/// drawable; routing that through the surface path is what left
/// the toolbar sitting on the buttons for a whole animation.
#[test]
fn a_moved_button_cluster_travels_on_its_own() {
    let prev = geom(2000, 1400, 2.0, 69.0);
    assert_eq!(resize_work(Some(prev), &geom(2000, 1400, 2.0, 0.0)), ResizeWork::ChromeOnly);
    // Measurement noise is not a move.
    assert_eq!(resize_work(Some(prev), &geom(2000, 1400, 2.0, 69.3)), ResizeWork::Nothing);
}
