//! Pane scrims: which reason wins, the attention ladder, and the focus
//! ring over a neighbour's scrim.

use super::*;
use crate::frame_build::color::rgba8_of_f32;
use crate::frame_build::frame::{
    attention_scrim, DRAG_SOURCE_SCRIM, EMPTY_SEAT_SCRIM, PARKED_SCRIM, RESTING_SCRIM,
    UNFOCUSED_SCRIM,
};

/// The drag source is the deepest reason of all: while a pane is
/// in the user's hand it has to read as picked up, deeper than
/// merely unfocused and deeper than an empty seat.
#[test]
fn the_drag_source_reads_deeper_than_the_other_reasons() {
    use crate::layout::Layout;
    let device = match system_default_device() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            return;
        }
    };
    let mut font = FontCache::build().expect("font");
    let mut atlas =
        new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
    let mut color_atlas =
        new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");
    let grid = crate::grid::Grid::new(10, 4);
    let layout = Layout::build(800.0, 600.0, 0.0, 0.0, 20.0, 1, 1, 8.0, 16.0);
    let view = SessionView {
        grid: &grid, view_offset: 0, cursor_visible: false, focused: true,
        title: "", selection: None, ime_preedit: "",
        right_badge: "", agent_tui: false, cwd: "", top_fixed_h_cells: 0, bot_fixed_h_cells: 0,
        highlight_spans: &[], search_overlay: None, history: Default::default(), session: 0, strip: None, seq: 0,
        dormant: false, recede: 0, scrim: 0.0,
    };
    let mut overlay_rects = Vec::new();
    build_instances(
        &layout,
        std::slice::from_ref(&view),
        &[],
        0,
        true,
        None, None,
        None, // settings panel
        None, None,
        Some(0), // this pane is being dragged
        None,
        &mut font,
        &mut atlas,
        &mut color_atlas,
        &mut Vec::new(),
        &mut Vec::new(),
        &mut Vec::new(),
        &mut Vec::new(),
        &mut Vec::new(),
        &mut crate::frame_build::frame::FrameKeep::default(),
        &mut Vec::new(),
        &mut Vec::new(),
        &mut overlay_rects,
    );
    // Alphas are eight bits now, so the scrim is compared as the
    // byte it becomes rather than as the float it was written as.
    let want = (DRAG_SOURCE_SCRIM * 255.0).round() as u8;
    let scrims: Vec<u8> = overlay_rects
        .iter()
        .filter(|r| r.fill.r == 0 && r.fill.a > 0)
        .map(|r| r.fill.a)
        .collect();
    assert!(
        scrims.contains(&want),
        "a dragged pane wears the drag scrim even while focused, got {scrims:?} \
         (wanted {want})"
    );
    const _: () = assert!(DRAG_SOURCE_SCRIM > EMPTY_SEAT_SCRIM);
}

/// The attention ladder, as a table.  One pane is fully present,
/// the rest step back, a resting one steps back further — and the
/// pane the user is in is never dimmed, whatever the state machine
/// thinks of it.
#[test]
fn the_attention_ladder_puts_the_focused_pane_in_front() {
    assert_eq!(attention_scrim(true, 0), 0.0, "the pane you are in");
    assert_eq!(
        attention_scrim(true, 2),
        0.0,
        "…even if its session was reclaimed: you are in it now"
    );
    assert_eq!(attention_scrim(false, 0), UNFOCUSED_SCRIM);
    assert_eq!(attention_scrim(false, 1), RESTING_SCRIM);
    assert_eq!(attention_scrim(false, 2), PARKED_SCRIM);
    const _: () = assert!(
        UNFOCUSED_SCRIM < RESTING_SCRIM && RESTING_SCRIM < PARKED_SCRIM,
        "the ladder has to be monotonic or it says nothing"
    );
    const _: () = assert!(
        PARKED_SCRIM > UNFOCUSED_SCRIM,
        "a pane whose program is gone has to read further away than \
         one that is merely not the focused pane"
    );
    // Opacity is what the user perceives; the constants are its
    // complement, and the tiers are the ones asked for.
    assert!((1.0 - UNFOCUSED_SCRIM - 0.75).abs() < 1e-6, "75 % opacity");
    assert!((1.0 - RESTING_SCRIM - 0.50).abs() < 1e-6, "50 % opacity");
    assert!((1.0 - PARKED_SCRIM - 0.25).abs() < 1e-6, "25 % opacity");
}

/// The scrim primitive is shared by every reason a pane recedes,
/// and they do not stack: two 0.22 layers composite to 0.39, a
/// shade nobody chose.
#[test]
fn scrims_do_not_stack_the_deepest_reason_wins() {
    use crate::layout::Layout;
    let device = match system_default_device() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            return;
        }
    };
    let mut font = FontCache::build().expect("font");
    let mut atlas =
        new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
    let mut color_atlas =
        new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");
    let grid = crate::grid::Grid::new(10, 4);
    let layout = Layout::build(800.0, 600.0, 0.0, 0.0, 20.0, 1, 1, 8.0, 16.0);
    let mk = |focused: bool, dormant: bool, recede: u32| SessionView {
        grid: &grid, view_offset: 0, cursor_visible: false, focused,
        title: "", selection: None, ime_preedit: "",
        right_badge: "", agent_tui: false, cwd: "", top_fixed_h_cells: 0, bot_fixed_h_cells: 0,
        highlight_spans: &[], search_overlay: None, history: Default::default(), session: 0, strip: None, seq: 0,
        dormant, recede, scrim: attention_scrim(focused, recede),
    };
    let mut scrim_alphas = |view: SessionView| -> Vec<u8> {
        let mut overlay_rects = Vec::new();
        build_instances(
            &layout,
            std::slice::from_ref(&view),
            &[],
            0,
            true,
            None, None, None, None, None, None, None,
            &mut font,
            &mut atlas,
            &mut color_atlas,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut crate::frame_build::frame::FrameKeep::default(),
            &mut Vec::new(),
            &mut Vec::new(),
            &mut overlay_rects,
        );
        overlay_rects
            .iter()
            .filter(|r| r.fill.r == 0 && r.fill.a > 0)
            .map(|r| r.fill.a)
            .collect()
    };
    // Alphas are eight bits on the way to the GPU, so the
    // constants are compared as the bytes they become. Comparing
    // the float back out and asking for 1e-6 asks the byte to
    // carry a precision it does not have: 0.75 comes back as
    // 191/255 = 0.7490196.
    let byte = |a: f32| (a * 255.0).round() as u8;
    // Focused and live: nothing over it at all.
    let focused = scrim_alphas(mk(true, false, 0));
    assert!(
        !focused.iter().any(|a| *a == byte(UNFOCUSED_SCRIM)),
        "the focused pane gets no attention scrim, got {focused:?}"
    );
    // Unfocused and parked: one scrim, the parked one.
    let parked = scrim_alphas(mk(false, false, 2));
    assert!(
        parked.iter().any(|a| *a == byte(PARKED_SCRIM)),
        "a parked pane recedes to the parked tier, got {parked:?}"
    );
    // Parked AND an empty seat: still one scrim, the deeper of
    // the two.
    let both = scrim_alphas(mk(false, true, 2));
    let deep = both.iter().filter(|a| **a >= byte(EMPTY_SEAT_SCRIM)).count();
    assert_eq!(deep, 1, "exactly one scrim, got {both:?}");
    assert!(both.iter().any(|a| *a == byte(PARKED_SCRIM)));
}

/// the right/bottom sides lies inside the NEIGHBOURING cells'
/// rects.  Every unfocused pane covers its whole rect with a scrim
/// in the overlay pass — which runs later — so those two sides
/// came out dimmed while left and top stayed bright (2026-08-20
/// report).  The fix repaints the ring into the overlay pass after
/// the scrims; this pins that it is still there.
#[test]
fn focus_ring_is_repainted_over_neighbour_scrims() {
    use crate::grid::Grid;
    use crate::layout::Layout;

    let device = match system_default_device() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            return;
        }
    };
    let mut font = FontCache::build().expect("font");
    let mut atlas =
        new_atlas(&device, 256, 256, false, font.font_table()).map(|(a, _)| a).expect("atlas");
    let mut color_atlas =
        new_atlas(&device, 256, 256, true, font.font_table()).map(|(a, _)| a).expect("color atlas");

    let grid = Grid::new(10, 4);
    // 2x2 with a gutter: gutter > 0 is what turns the ring on.
    let layout = Layout::build(
        font.cell_w * 24.0,
        font.cell_h * 10.0,
        0.0,
        0.0,
        2.0,
        2,
        2,
        font.cell_w,
        font.cell_h,
    );
    let mk = |focused: bool, scrim: f32| SessionView {
        grid: &grid,
        view_offset: 0,
        cursor_visible: true,
        focused,
        title: "",
        selection: None,
        ime_preedit: "",
        dormant: false,
        recede: 0,
        scrim,
        right_badge: "", agent_tui: false, cwd: "",
        top_fixed_h_cells: 0,
        bot_fixed_h_cells: 0,
        highlight_spans: &[],
        search_overlay: None,
        history: Default::default(), session: 0, strip: None,
        seq: 0,
    };
    // Pane 0 focused; its right / bottom / bottom-right neighbours
    // all carry a scrim, which is the situation that hid the ring.
    let views = vec![mk(true, 0.0), mk(false, 0.35), mk(false, 0.35), mk(false, 0.35)];

    let mut overlay_ui: Vec<UiRectInstance> = Vec::new();
    build_instances(
        &layout, &views, &[], 0, true,
        None, None, None, None, None, None, None,
        &mut font, &mut atlas, &mut color_atlas,
        // cells, glyphs, color_glyphs, _dots
        &mut Vec::new(), &mut Vec::new(), &mut Vec::new(), &mut Vec::new(),
        // ui_rects, pane_caches
        &mut Vec::new(), &mut crate::frame_build::frame::FrameKeep::default(),
        // overlay_cells, overlay_glyphs, overlay_ui_rects
        &mut Vec::new(), &mut Vec::new(), &mut overlay_ui,
    );

    let ring = rgba8_of_f32([0.72, 0.76, 0.82, 1.0]);
    let ring_at: Vec<usize> = overlay_ui
        .iter()
        .enumerate()
        .filter(|(_, r)| r.fill == ring)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(ring_at.len(), 8, "expected the 8 ring rects in the overlay pass, got {ring_at:?}");

    // Order is what makes it visible: every scrim must be pushed
    // before the ring, or the ring is dimmed again.
    let last_scrim = overlay_ui
        .iter()
        .rposition(|r| {
            r.fill.a > 0 && r.fill.r == 0 && r.fill.g == 0 && r.fill.b == 0
        })
        .expect("unfocused panes should have pushed scrims");
    assert!(
        ring_at[0] > last_scrim,
        "ring must be painted after the scrims (ring at {ring_at:?}, last scrim {last_scrim})"
    );
}
