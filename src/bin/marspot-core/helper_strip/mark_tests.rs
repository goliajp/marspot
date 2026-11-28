//! The `+N` after a pane's title and the strip under it, pressed where
//! they are drawn, at both densities.  While the strip is drawn the
//! title leaves the count out and the strip's rows reach the panes;
//! with the strip turned off the count is drawn and a press on it
//! opens the helper panel.  The dev seam's spot for the count is the
//! one a press hits.

use super::super::host_tests::{frame, say, turned_on};
use super::super::view_tests::click;
use super::super::*;
use super::plain_tests::{plain, sized_to_view, with_plain};
use crate::boot_assembly_tests::Sandbox;
use marspot::shell_proto::HelperStrip;
use marspot::ui::components::panel_script;

/// Draw a frame of the window, offscreen: the title's `+N` is pressed
/// where the last frame drew it.
fn publish(app: &mut CoreApp) {
    let (w, h) = (app.windows[0].w_phys as usize, app.windows[0].h_phys as usize);
    let surface = marspot::iosurface::IOSurface::create(w, h).unwrap();
    let tex = surface.make_metal_texture(app.renderer.device()).unwrap();
    app.render_blocking(0, &tex);
}

/// Every point of the first pane's title the chrome reads as `want`.
fn on_title(app: &mut CoreApp, want: ChromeTarget) -> usize {
    let c = app.windows[0].layout.cells[0];
    let y = c.y_top + 4.0;
    (0..c.w as usize).filter(|dx| app.chrome_at(0, c.x + *dx as f64, y) == Some(want)).count()
}

/// Where the dev seam would click for `click:helpers-mark`.
fn dev_mark(app: &mut CoreApp) -> Option<(f64, f64)> {
    let steps = panel_script::parse("click:helpers-mark").unwrap();
    app.dev_panels = Some(PanelScript::new(steps, Instant::now()));
    app.dev_helper_spots(0)[0]
}

fn pressed_where_drawn(scale: f64) {
    marspot::ui::set_chrome_scale(scale);
    let sb = Sandbox::new();
    let (mut app, mut s, mut p) = with_plain(&sb);
    assert_eq!(app.windows[0].scale, scale);
    // a lone pane has no title, so no +N either
    app.windows[0].panes.push(Pane::new_vacant(s.sid + 100, 60, 16));
    app.rebuild_layout(0);
    assert!(app.windows[0].layout.cell_title_h > 0.0);
    say(&mut app, s.sid, vec![plain(p.sid)]);
    app.pane_control.strips.take(HelperStrip::all_told());
    frame(&mut app, &mut s, b"$ ");
    assert!(app.strips.drawn(s.sid));
    publish(&mut app);
    assert_eq!(app.shadows_counted(s.sid), 1, "the strip's row is counted");
    assert!(on_title(&mut app, ChromeTarget::Title(0)) > 0, "the title is where it is looked for");
    assert_eq!(on_title(&mut app, ChromeTarget::Helpers(0)), 0, "the strip is drawn: the title has no +N");
    assert_eq!(dev_mark(&mut app), None, "so the seam has no spot for it");
    click(&mut app, s.sid, 2);
    sized_to_view(&mut app, &mut p);
    assert_eq!(app.windows[0].panes[0].peeked(), Some(p.sid), "the row reaches the pane");
    app.place_strips(0);
    click(&mut app, s.sid, 1);
    assert_eq!(app.windows[0].panes[0].peeked(), None);

    turned_on(false);
    app.strips_tick();
    assert!(!app.strips.drawn(s.sid), "turned off");
    publish(&mut app);
    assert!(on_title(&mut app, ChromeTarget::Helpers(0)) > 0, "the title counts the pane");
    let (x, y) = dev_mark(&mut app).expect("the seam finds the +N");
    assert_eq!(app.chrome_at(0, x, y), Some(ChromeTarget::Helpers(0)));
    assert_eq!(app.windows[0].panels.open(), None);
    app.mouse_down(0, x, y, Modifiers::default());
    app.mouse_up(0, app.windows[0].window_id, x, y);
    assert_eq!(app.windows[0].panels.open(), Some(SurfaceKind::Helpers), "a press on the +N opens the panel");
}

#[test]
fn helper_strip_the_mark_and_the_strip_are_pressed_where_drawn_at_1x() {
    pressed_where_drawn(1.0);
}

#[test]
fn helper_strip_the_mark_and_the_strip_are_pressed_where_drawn_at_2x() {
    pressed_where_drawn(2.0);
}
