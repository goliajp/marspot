//! A node that floats over its surface: where it stands, what it is
//! drawn over, what it is found before, and what does not hold it.

use golia_ui_core::{
    Action, FakeMeasure, Ink, Inputs, Length as L, Metrics, NodeId, NodeKind, Op, Recording, Rect, Surface, Ui,
    WidgetId,
};

const MEASURE: FakeMeasure = FakeMeasure::new(1.0);
const UNDER: Ink = Ink::Role(1);
const OVER: Ink = Ink::Role(2);
const TOP: Ink = Ink::Role(3);

fn inputs() -> Inputs {
    Inputs {
        data_rev: 1,
        theme_rev: 1,
        origin: (100.0, 50.0),
        avail: (200.0, 100.0),
        metrics: Metrics { px_per_pt: 1.0, cell_w: 8.0, cell_h: 16.0, cell_ascent: 12.0 },
        font_rev: 1,
        atlas_gen: 1,
    }
}

/// Two rows of 40 in a clipping, scrolling column, the first holding a
/// float described between them; the float stands at `at`.
fn sheet(at: (f64, f64)) -> impl Fn(&mut Ui<'_>) {
    move |ui| {
        ui.scroll(0, |ui| {
            ui.leaf(0, L::Px(200.0), L::Px(40.0)).fill(UNDER).widget(WidgetId(1), Action::new(1, 1));
            ui.float(at.0, at.1, |ui| {
                ui.leaf(0, L::Px(60.0), L::Px(30.0)).fill(OVER).widget(WidgetId(9), Action::new(9, 9));
            });
            ui.leaf(0, L::Px(200.0), L::Px(40.0)).fill(UNDER).widget(WidgetId(2), Action::new(1, 2));
            ui.leaf(0, L::Px(200.0), L::Px(40.0)).fill(UNDER).widget(WidgetId(3), Action::new(1, 3));
        })
        .clip();
    }
}

fn surface(at: (f64, f64)) -> Surface<Recording> {
    let mut s = Surface::new(Recording::new());
    s.frame(&inputs(), &MEASURE, sheet(at));
    s
}

fn node_of(s: &Surface<Recording>, kind: NodeKind) -> Rect {
    let i = s.tree().nodes().iter().position(|n| n.kind == kind).expect("the node");
    s.layout().rect(NodeId(i as u32))
}

fn hit(s: &Surface<Recording>, p: (f64, f64)) -> Option<u32> {
    s.hit(p).map(|h| h.action.arg)
}

#[test]
fn a_float_takes_no_room_and_stands_where_it_is_told_inside_the_surface_or_out_of_it() {
    for at in [(20.0, 5.0), (0.0, 0.0), (-30.0, -12.5), (500.0, 300.0)] {
        let s = surface(at);
        let f = node_of(&s, NodeKind::Float);
        assert_eq!(f, Rect::new(100.0 + at.0, 50.0 + at.1, 60.0, 30.0), "told to stand at {at:?}");
        // its child is in it
        let child = s.tree().nodes().iter().position(|n| n.widget == Some(WidgetId(9))).unwrap();
        assert_eq!(s.layout().rect(NodeId(child as u32)), f);
        // the rows are where they are without it: the second starts where the first ends
        let rows: Vec<Rect> = [1, 2, 3]
            .iter()
            .map(|id| {
                let i = s.tree().nodes().iter().position(|n| n.widget == Some(WidgetId(*id))).unwrap();
                s.layout().rect(NodeId(i as u32))
            })
            .collect();
        assert_eq!(rows[0], Rect::new(100.0, 50.0, 200.0, 40.0));
        assert_eq!(rows[1], Rect::new(100.0, 90.0, 200.0, 40.0), "the float took room");
        assert_eq!(s.layout().scroll_extent(0).content_h, 120.0);
    }
}

#[test]
fn a_float_is_painted_after_everything_and_outside_the_clip_it_was_described_in() {
    let s = surface((20.0, 5.0));
    let ops = &s.output().ops;
    let pop = ops.iter().position(|op| matches!(op, Op::PopClip)).expect("the column clips");
    let over = ops
        .iter()
        .position(|op| matches!(op, Op::Rounded { fill: Some(ink), .. } if *ink == OVER))
        .expect("the float is painted");
    assert!(over > pop, "the float was painted inside the clip");
    let last_under = ops.iter().rposition(|op| matches!(op, Op::Rounded { fill: Some(ink), .. } if *ink == UNDER));
    assert!(over > last_under.unwrap(), "something was painted over the float");
    // out of the surface altogether it is still painted, whole
    let s = surface((500.0, 300.0));
    let rect = s.output().ops.iter().find_map(|op| match op {
        Op::Rounded { rect, fill: Some(ink), .. } if *ink == OVER => Some(*rect),
        _ => None,
    });
    assert_eq!(rect, Some(Rect::new(600.0, 350.0, 60.0, 30.0)));
}

#[test]
fn a_float_is_found_before_what_it_covers_and_where_the_surface_is_not() {
    let s = surface((20.0, 5.0));
    // over the first row: the float, not the row
    assert_eq!(hit(&s, (130.0, 60.0)), Some(9));
    // beside it the row answers
    assert_eq!(hit(&s, (110.0, 60.0)), Some(1));
    assert_eq!(hit(&s, (130.0, 100.0)), Some(2), "the row under where the float was described");
    // outside the surface, outside every ancestor's box
    let s = surface((-30.0, -12.5));
    assert_eq!(hit(&s, (75.0, 40.0)), Some(9));
    assert_eq!(hit(&s, (75.0, 80.0)), None);
    let s = surface((500.0, 300.0));
    assert_eq!(hit(&s, (610.0, 360.0)), Some(9));
}

#[test]
fn a_scroll_moves_what_is_under_a_float_and_not_the_float() {
    let mut s = surface((20.0, 5.0));
    assert!(s.wheel((110.0, 100.0), 20.0), "120 of rows in 100 of viewport scroll");
    s.frame(&inputs(), &MEASURE, |_| panic!("a scroll describes nothing"));
    assert_eq!(hit(&s, (130.0, 60.0)), Some(9), "the float went with the rows");
    assert_eq!(hit(&s, (110.0, 75.0)), Some(2), "the rows did not move");
    let over = s.output().ops.iter().find_map(|op| match op {
        Op::Rounded { rect, fill: Some(ink), .. } if *ink == OVER => Some(*rect),
        _ => None,
    });
    assert_eq!(over, Some(Rect::new(120.0, 55.0, 60.0, 30.0)));
}

#[test]
fn of_two_floats_the_one_described_later_is_on_top_and_a_dragged_surface_takes_them_along() {
    let mut s = Surface::new(Recording::new());
    s.frame(&inputs(), &MEASURE, |ui| {
        ui.vstack(|ui| {
            ui.float(10.0, 10.0, |ui| {
                ui.leaf(0, L::Px(60.0), L::Px(30.0)).fill(OVER).widget(WidgetId(8), Action::new(9, 8));
            });
            ui.leaf(0, L::Px(200.0), L::Px(100.0)).fill(UNDER).widget(WidgetId(1), Action::new(1, 1));
            ui.float(40.0, 20.0, |ui| {
                ui.leaf(0, L::Px(60.0), L::Px(30.0)).fill(TOP).widget(WidgetId(9), Action::new(9, 9));
            });
        });
    });
    // where the two overlap
    assert_eq!(hit(&s, (150.0, 75.0)), Some(9));
    assert_eq!(hit(&s, (115.0, 65.0)), Some(8));
    let fills: Vec<Ink> = s
        .output()
        .ops
        .iter()
        .filter_map(|op| if let Op::Rounded { fill: Some(ink), .. } = op { Some(*ink) } else { None })
        .collect();
    assert_eq!(fills, [UNDER, OVER, TOP], "painted in the order they lie");

    s.set_offset((7.0, -3.0));
    assert_eq!(hit(&s, (157.0, 72.0)), Some(9));
    assert_eq!(hit(&s, (212.0, 72.0)), Some(1), "past the float that moved with it");
}
