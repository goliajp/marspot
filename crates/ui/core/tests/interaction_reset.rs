//! State the application sets from outside: a scroll offset it keeps
//! itself, and a surface put back to how it is when first shown.

use golia_ui_core::{
    Action, FakeMeasure, Inputs, Key, Length as L, Metrics, NoOut, Recording, Surface, Ui, WidgetId,
};

const MEASURE: FakeMeasure = FakeMeasure::new(1.0);

fn inputs() -> Inputs {
    Inputs {
        data_rev: 1,
        theme_rev: 1,
        origin: (0.0, 0.0),
        avail: (200.0, 100.0),
        metrics: Metrics { px_per_pt: 1.0, cell_w: 8.0, cell_h: 16.0, cell_ascent: 12.0 },
        font_rev: 1,
        atlas_gen: 1,
    }
}

/// Ten rows of 40 in a viewport of 100.
fn list(ui: &mut Ui<'_>) {
    ui.scroll(0, |ui| {
        for k in 0..10u32 {
            ui.leaf(0, L::Px(200.0), L::Px(40.0)).widget(WidgetId(k), Action::new(1, k));
        }
    });
}

fn never(_: &mut Ui<'_>) {
    panic!("nothing asked for the tree to be described again");
}

#[test]
fn an_offset_set_from_outside_repaints_and_is_held_to_the_content_at_the_next_layout() {
    let mut s = Surface::new(NoOut);
    let mut kept = Recording::new();
    let i = inputs();
    assert!(s.prepare(&i, &MEASURE, list));
    s.paint_into(&mut kept);

    s.interaction.set_scroll(0, 120.0);
    assert_eq!(s.interaction.scroll[0], 120.0);
    assert!(s.prepare(&i, &MEASURE, never), "a scroll is a repaint");
    s.paint_into(&mut kept);
    s.interaction.set_scroll(0, 120.0);
    assert!(!s.prepare(&i, &MEASURE, never), "the same offset again is nothing");

    // past the end, then a layout: 400 of content in 100 of viewport
    s.interaction.set_scroll(0, 5000.0);
    let taller = Inputs { avail: (200.0, 120.0), ..i };
    assert!(s.prepare(&taller, &MEASURE, never));
    assert_eq!(s.interaction.scroll[0], 280.0);
    s.interaction.set_scroll(0, -3.0);
    assert_eq!(s.interaction.scroll[0], 0.0);
}

#[test]
fn a_surface_put_back_forgets_everything_and_is_never_taken_for_painted() {
    let mut s = Surface::new(NoOut);
    let mut kept = Recording::new();
    let i = inputs();
    s.prepare(&i, &MEASURE, list);
    // three changes, painted
    s.pointer_move((10.0, 10.0));
    s.key(Key::Tab);
    s.interaction.set_scroll(0, 40.0);
    assert!(s.prepare(&i, &MEASURE, never));
    s.paint_into(&mut kept);
    let painted_at = s.interaction.revision();

    s.interaction.reset();
    assert_eq!(
        (s.interaction.hovered, s.interaction.focused, s.interaction.focus_visible, s.interaction.scroll[0]),
        (None, None, false, 0.0)
    );
    assert!(s.interaction.revision() > painted_at, "the revision started again");
    // as many changes as before the reset: a revision that restarted
    // would now equal the one the picture was painted at
    s.pointer_move((10.0, 10.0));
    s.key(Key::Tab);
    s.interaction.set_scroll(0, 40.0);
    assert!(s.interaction.revision() > painted_at);
    assert!(s.prepare(&i, &MEASURE, never), "a stale picture was taken for current");
}
