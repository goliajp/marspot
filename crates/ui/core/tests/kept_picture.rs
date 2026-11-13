//! A surface whose picture is kept by its caller, and the two small
//! things a panel needs from a node: a press without a Tab stop, and a
//! preset label made at run time.

use golia_ui_core::{
    Action, Counters, FakeMeasure, FontSpec, Ink, Inputs, Key, KeyResult, Length as L, Metrics, NoOut, Op,
    Recording, Surface, TextFont, Ui, WidgetId,
};

const MEASURE: FakeMeasure = FakeMeasure::new(2.0);
const LABEL: TextFont = TextFont::Ui(FontSpec { size_q: 40, weight: 400, features: 0 });

fn inputs() -> Inputs {
    Inputs {
        data_rev: 1,
        theme_rev: 1,
        origin: (0.0, 0.0),
        avail: (400.0, 300.0),
        metrics: Metrics { px_per_pt: 2.0, cell_w: 8.0, cell_h: 16.0, cell_ascent: 12.0 },
        font_rev: 1,
        atlas_gen: 1,
    }
}

fn rows(ui: &mut Ui<'_>) {
    ui.vstack(|ui| {
        for k in 0..4u32 {
            ui.hstack(|ui| {
                ui.text(LABEL, Ink::Role(0), "label");
            })
            .width(L::Pct(1.0))
            .height(L::Pt(20.0))
            .fill(Ink::Role(1))
            .fill_hover(Ink::Role(2))
            .widget(WidgetId(k), Action::new(1, k));
        }
    });
}

fn counts(build: u64, layout: u64, paint: u64) -> Counters {
    Counters { build, layout, paint }
}

fn never(_: &mut Ui<'_>) {
    panic!("nothing asked for the tree to be described again");
}

#[test]
fn a_picture_kept_by_the_caller_is_painted_only_when_prepare_says_it_is_stale() {
    let mut s = Surface::new(NoOut);
    let mut kept = Recording::new();
    let i = inputs();

    assert!(s.prepare(&i, &MEASURE, rows), "nothing has been painted yet");
    s.paint_into(&mut kept);
    assert_eq!(s.counters(), counts(1, 1, 1));
    let first = kept.clone();
    assert!(!first.ops.is_empty());

    assert!(!s.prepare(&i, &MEASURE, never), "an idle frame");
    assert_eq!(s.counters(), counts(1, 1, 1));

    // prepare paints nothing: the picture is stale until someone paints
    assert!(s.pointer_move((10.0, 50.0)));
    assert!(s.prepare(&i, &MEASURE, never));
    assert!(s.prepare(&i, &MEASURE, never), "still stale: nobody painted");
    s.paint_into(&mut kept);
    assert_eq!(s.counters(), counts(1, 1, 2));
    assert_ne!(kept, first, "the hovered row changed fill");
    assert!(!s.prepare(&i, &MEASURE, never));

    // a new atlas: kept glyphs point into the old one
    let moved = Inputs { atlas_gen: 2, ..i };
    assert!(s.prepare(&moved, &MEASURE, never));
    s.paint_into(&mut kept);
    assert_eq!(s.counters(), counts(1, 1, 3));
    assert!(!s.prepare(&moved, &MEASURE, never));

    // a new size: laid out again, and stale whatever was painted before
    let wider = Inputs { avail: (500.0, 300.0), ..moved };
    assert!(s.prepare(&wider, &MEASURE, never));
    assert_eq!(s.counters(), counts(1, 2, 3));
}

#[test]
fn a_pressable_node_is_hit_and_is_not_a_tab_stop() {
    let mut s = Surface::new(Recording::new());
    s.frame(&inputs(), &MEASURE, |ui| {
        ui.vstack(|ui| {
            ui.leaf(0, L::Px(400.0), L::Px(40.0)).pressable(WidgetId(7), Action::new(9, 0));
            ui.leaf(0, L::Px(400.0), L::Px(40.0)).widget(WidgetId(8), Action::new(9, 1));
        });
    });
    let rev = s.interaction.revision();
    let hit = s.hit((10.0, 10.0)).expect("the handle is there");
    assert_eq!((hit.target.id, hit.action), (WidgetId(7), Action::new(9, 0)));
    assert_eq!(s.hit((10.0, 90.0)), None);
    assert_eq!(s.interaction.revision(), rev, "a lookup is not a hover");
    for _ in 0..2 {
        assert_eq!(s.key(Key::Tab), KeyResult::Handled);
        assert_eq!(s.interaction.focused.map(|t| t.id), Some(WidgetId(8)), "Tab never stops on the handle");
    }
}

#[test]
fn a_label_copied_into_the_tree_reaches_the_preset() {
    let mut s = Surface::new(Recording::new());
    let n = 42;
    let out = s.frame(&inputs(), &MEASURE, |ui| {
        ui.leaf(5, L::Px(30.0), L::Px(20.0)).label_copy(&format!("{n}"));
    });
    let leaf = out.ops.iter().find(|op| matches!(op, Op::Leaf { .. })).expect("a leaf");
    assert_eq!(out.text(leaf), "42");
}
