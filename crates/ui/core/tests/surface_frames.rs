//! What a surface does in a frame, and what it leaves alone.
//!
//! Every "did not run" below sits next to the change that makes the
//! same counter move, so a counter that never moves cannot pass.

use golia_ui_core::{
    Action, Counters, FakeMeasure, FontSpec, GroupKind, Ink, Inputs, Key, Length as L, Metrics, Op,
    Recording, Rect, Surface, TextFont, Ui, WidgetId, WidgetState,
};

const MEASURE: FakeMeasure = FakeMeasure::new(2.0);
const LABEL: TextFont = TextFont::Ui(FontSpec { size_q: 40, weight: 400, features: 0 });
const INK: Ink = Ink::Role(0);
const ROW: Ink = Ink::Role(1);
const ROW_HOVER: Ink = Ink::Role(2);
const ROW_PRESSED: Ink = Ink::Role(3);

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

/// Four rows, 40 px each, each a widget with a label and a number.
fn rows(ui: &mut Ui<'_>) {
    ui.vstack(|ui| {
        for k in 0..4u32 {
            ui.hstack(|ui| {
                ui.text(LABEL, INK, "label");
                ui.spacer();
                ui.text_fmt(LABEL, INK, format_args!("row {k}"));
            })
            .width(L::Pct(1.0))
            .height(L::Pt(20.0))
            .fill(ROW)
            .fill_hover(ROW_HOVER)
            .fill_pressed(ROW_PRESSED)
            .widget(WidgetId(k), Action::new(1, k));
        }
    });
}

fn counts(build: u64, layout: u64, paint: u64) -> Counters {
    Counters { build, layout, paint }
}

fn fills(r: &Recording) -> Vec<Ink> {
    r.ops
        .iter()
        .filter_map(|op| if let Op::Rounded { fill: Some(ink), .. } = op { Some(*ink) } else { None })
        .collect()
}

#[test]
fn the_first_frame_runs_every_stage_once() {
    let mut s = Surface::new(Recording::new());
    s.frame(&inputs(), &MEASURE, rows);
    assert_eq!(s.counters(), counts(1, 1, 1));
    assert_eq!(s.tree().len(), 1 + 4 * 4);
    assert_eq!(fills(s.output()), [ROW; 4]);
}

#[test]
fn an_idle_frame_runs_nothing_and_is_the_kept_picture() {
    let mut s = Surface::new(Recording::new());
    let first = s.frame(&inputs(), &MEASURE, rows).clone();
    assert!(first.ops.len() >= 12, "four fills and eight runs of text: {}", first.ops.len());

    for _ in 0..3 {
        let mut described = false;
        let again = s.frame(&inputs(), &MEASURE, |ui| {
            described = true;
            rows(ui);
        });
        assert_eq!(*again, first, "the kept output is the frame");
        assert!(!described, "an idle frame does not describe the tree");
    }
    assert_eq!(s.counters(), counts(1, 1, 1), "nothing was built, laid out or walked");

    // the same counters do move when something is stale
    let moved = Inputs { data_rev: 2, ..inputs() };
    s.frame(&moved, &MEASURE, rows);
    assert_eq!(s.counters(), counts(2, 2, 2));
}

#[test]
fn a_hover_repaints_and_does_nothing_else() {
    let mut s = Surface::new(Recording::new());
    s.frame(&inputs(), &MEASURE, rows);

    assert!(s.pointer_move((10.0, 50.0)), "onto the second row");
    s.frame(&inputs(), &MEASURE, rows);
    assert_eq!(s.counters(), counts(1, 1, 2), "paint only");
    assert_eq!(fills(s.output()), [ROW, ROW_HOVER, ROW, ROW]);

    assert!(!s.pointer_move((20.0, 55.0)), "still the second row");
    s.frame(&inputs(), &MEASURE, rows);
    assert_eq!(s.counters(), counts(1, 1, 2), "no change, no repaint");

    s.pointer_down((20.0, 55.0));
    s.frame(&inputs(), &MEASURE, rows);
    assert_eq!(s.counters(), counts(1, 1, 3));
    assert_eq!(fills(s.output()), [ROW, ROW_PRESSED, ROW, ROW]);
    assert_eq!(s.pointer_up((20.0, 55.0)), Some(Action::new(1, 1)));
    s.frame(&inputs(), &MEASURE, rows);
    assert_eq!(fills(s.output()), [ROW, ROW_HOVER, ROW, ROW]);
}

#[test]
fn a_new_size_lays_out_and_repaints_without_describing() {
    let mut s = Surface::new(Recording::new());
    s.frame(&inputs(), &MEASURE, rows);
    let wide = |r: &Recording| match r.ops[0] {
        Op::Rounded { rect, .. } => rect.w,
        other => panic!("the first op is the first row's fill, not {other:?}"),
    };
    assert_eq!(wide(s.output()), 400.0);

    let narrower = Inputs { avail: (320.0, 300.0), ..inputs() };
    s.frame(&narrower, &MEASURE, rows);
    assert_eq!(s.counters(), counts(1, 2, 2), "the tree is the one already described");
    assert_eq!(wide(s.output()), 320.0);

    let denser = Inputs { metrics: Metrics { px_per_pt: 3.0, ..narrower.metrics }, ..narrower };
    s.frame(&denser, &MEASURE, rows);
    assert_eq!(s.counters(), counts(1, 3, 3), "a new scale is a new layout");

    let refonted = Inputs { font_rev: 2, ..denser };
    s.frame(&refonted, &MEASURE, rows);
    assert_eq!(s.counters(), counts(1, 4, 4));

    let moved = Inputs { origin: (10.0, 10.0), ..refonted };
    s.frame(&moved, &MEASURE, rows);
    assert_eq!(s.counters(), counts(1, 5, 5));
}

#[test]
fn each_input_makes_stale_only_what_depends_on_it() {
    let mut s = Surface::new(Recording::new());
    s.frame(&inputs(), &MEASURE, rows);

    let reatlased = Inputs { atlas_gen: 2, ..inputs() };
    s.frame(&reatlased, &MEASURE, rows);
    assert_eq!(s.counters(), counts(1, 1, 2), "kept glyphs point into the old atlas");

    let rethemed = Inputs { theme_rev: 2, ..reatlased };
    s.frame(&rethemed, &MEASURE, rows);
    assert_eq!(s.counters(), counts(2, 2, 3));

    s.set_offset((30.0, 0.0));
    s.frame(&rethemed, &MEASURE, rows);
    assert_eq!(s.counters(), counts(2, 2, 4), "a drag moves the picture, not the layout");
    assert!(
        matches!(s.output().ops[0], Op::Rounded { rect: Rect { x, .. }, .. } if x == 30.0),
        "{:?}",
        s.output().ops[0]
    );
}

#[test]
fn a_scroll_repaints_whole_rows_and_cuts_the_card_to_the_viewport() {
    const CARD: Ink = Ink::Role(5);
    let mut s = Surface::new(Recording::new());
    let describe = |ui: &mut Ui<'_>| {
        ui.scroll(0, |ui| {
            ui.vstack(|ui| {
                for k in 0..10u32 {
                    ui.leaf(7, L::Px(200.0), L::Px(30.0)).leaf(7, [k, 0]);
                }
            })
            .fill(CARD)
            .radius(L::Px(6.0));
        });
    };
    let small = Inputs { avail: (400.0, 100.0), ..inputs() };
    s.frame(&small, &MEASURE, describe);
    let shown = |r: &Recording| -> Vec<(u32, f64)> {
        r.ops
            .iter()
            .filter_map(|op| match op {
                Op::Leaf { data, rect, .. } => Some((data[0], rect.y)),
                _ => None,
            })
            .collect()
    };
    let card = |r: &Recording| match r.ops[0] {
        Op::Rounded { rect, radius, fill, .. } => (rect, radius, fill),
        other => panic!("the card comes first, not {other:?}"),
    };
    assert_eq!(shown(s.output()), [(0, 0.0), (1, 30.0), (2, 60.0)], "row 3 is cut at 100");
    assert_eq!(card(s.output()), (Rect::new(0.0, 0.0, 200.0, 100.0), 6.0, Some(CARD)));

    assert!(s.wheel((10.0, 10.0), 20.0));
    s.frame(&small, &MEASURE, describe);
    assert_eq!(s.counters(), counts(1, 1, 2), "scrolling lays nothing out");
    assert_eq!(shown(s.output()), [(1, 10.0), (2, 40.0), (3, 70.0)]);
    assert_eq!(card(s.output()).0, Rect::new(0.0, 0.0, 200.0, 100.0));

    // a shorter window pulls the offset back inside what is left
    assert!(s.wheel((10.0, 10.0), 1.0e6));
    assert_eq!(s.interaction.scroll[0], 200.0);
    let taller = Inputs { avail: (400.0, 250.0), ..inputs() };
    s.frame(&taller, &MEASURE, describe);
    assert_eq!(s.interaction.scroll[0], 50.0);
}

#[test]
fn the_focus_ring_is_painted_after_a_key_and_not_after_a_press() {
    let mut s = Surface::new(Recording::new());
    s.frame(&inputs(), &MEASURE, rows);
    let ring = |r: &Recording| {
        r.ops.iter().find_map(|op| if let Op::FocusRing { rect, .. } = op { Some(*rect) } else { None })
    };
    assert_eq!(ring(s.output()), None);

    s.key(Key::Tab);
    s.key(Key::Tab);
    s.frame(&inputs(), &MEASURE, rows);
    assert_eq!(s.counters(), counts(1, 1, 2), "focus is a repaint");
    assert_eq!(ring(s.output()), Some(Rect::new(0.0, 40.0, 400.0, 40.0)), "round the second row");

    s.pointer_down((10.0, 130.0));
    s.frame(&inputs(), &MEASURE, rows);
    assert_eq!(ring(s.output()), None);
}

#[test]
fn a_preset_is_told_which_of_its_items_the_pointer_and_the_focus_are_on() {
    let mut s = Surface::new(Recording::new());
    let describe = |ui: &mut Ui<'_>| {
        ui.group(WidgetId(9), GroupKind::Segmented, false, |ui| {
            for k in 0..3u32 {
                ui.leaf(0, L::Px(50.0), L::Px(30.0)).item(Action::new(4, k));
            }
        })
        .leaf(2, [0, 0])
        .label("segments")
        .clip();
    };
    s.frame(&inputs(), &MEASURE, describe);
    let state = |r: &Recording| {
        r.ops
            .iter()
            .find_map(|op| match op {
                Op::Leaf { tag: 2, state, rect, .. } => Some((*state, *rect, r.text(op).to_string())),
                _ => None,
            })
            .expect("the preset was painted")
    };
    let (idle, rect, label) = state(s.output());
    assert_eq!(idle, WidgetState::IDLE);
    assert_eq!(rect, Rect::new(0.0, 0.0, 150.0, 30.0));
    assert_eq!(label, "segments");
    assert!(
        matches!(s.output().ops[..], [Op::Leaf { .. }, Op::PushClip(_), Op::PopClip]),
        "the items draw nothing of their own, inside the clip: {:?}",
        s.output().ops
    );

    s.pointer_move((120.0, 10.0));
    s.frame(&inputs(), &MEASURE, describe);
    let (hover, ..) = state(s.output());
    assert!(hover.hovered && hover.hovered_item == 2 && !hover.focused, "{hover:?}");

    s.key(Key::Tab);
    assert_eq!(s.key(Key::Right), golia_ui_core::KeyResult::Action(Action::new(4, 1)));
    s.frame(&inputs(), &MEASURE, describe);
    let (focus, ..) = state(s.output());
    assert!(focus.focused && focus.focused_item == 1 && focus.hovered_item == 2, "{focus:?}");
    // the ring goes round the item, not the control
    assert!(s.output().ops.contains(&Op::FocusRing { rect: Rect::new(50.0, 0.0, 50.0, 30.0), radius: 0.0 }));
}

#[test]
fn fills_go_to_the_rounded_pass_unless_a_node_says_flat_and_a_rule_is_inset_by_its_padding() {
    use golia_ui_core::Edges;
    let mut s = Surface::new(Recording::new());
    s.frame(&inputs(), &MEASURE, |ui| {
        ui.vstack(|ui| {
            ui.leaf(0, L::Px(200.0), L::Px(30.0)).fill(Ink::Role(7)).flat();
            ui.rule(L::Px(1.0), Ink::Role(8)).padding(Edges::xy(L::Px(12.0), L::ZERO));
            ui.leaf(0, L::Px(200.0), L::Px(30.0)).fill(Ink::Role(9));
        });
    });
    assert_eq!(
        s.output().ops,
        [
            Op::Solid { rect: Rect::new(0.0, 0.0, 200.0, 30.0), ink: Ink::Role(7) },
            Op::Rounded {
                rect: Rect::new(12.0, 30.0, 176.0, 1.0),
                radius: 0.0,
                fill: Some(Ink::Role(8)),
                border: None,
            },
            Op::Rounded {
                rect: Rect::new(0.0, 30.0, 200.0, 30.0),
                radius: 0.0,
                fill: Some(Ink::Role(9)),
                border: None,
            },
        ]
    );
}
