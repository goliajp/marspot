//! A column of rows of one height, each with a spot: which row a point
//! is on is worked out from its height, the spot answers with its own
//! action, and the pointer moving onto it or off it is a change.

use golia_ui_core::{
    Action, FakeMeasure, Inputs, Length as L, Metrics, Op, Recording, Rect, SPOT, Surface, Target, Ui, WidgetId,
};

const MEASURE: FakeMeasure = FakeMeasure::new(1.0);
const ROWS: u32 = 12;
const ROW_H: f64 = 22.0;
/// The close mark at the end of each row, from the row's corner.
const MARK: Rect = Rect::new(170.0, 4.0, 14.0, 14.0);

fn inputs() -> Inputs {
    Inputs {
        data_rev: 1,
        theme_rev: 1,
        origin: (0.0, 0.0),
        avail: (800.0, 600.0),
        metrics: Metrics { px_per_pt: 1.0, cell_w: 8.0, cell_h: 16.0, cell_ascent: 12.0 },
        font_rev: 1,
        atlas_gen: 1,
    }
}

/// A button over the column, then the column at (0, 70): 200 wide,
/// `ROWS` rows `ROW_H` high, each with the mark as its spot.  With
/// `rows` off the column is an ordinary one, walked row by row.
fn column(rows: bool) -> impl Fn(&mut Ui<'_>) {
    move |ui| {
        ui.vstack(|ui| {
            ui.float(10.0, 20.0, |ui| {
                ui.leaf(7, L::Px(40.0), L::Px(30.0)).pressable(WidgetId(100), Action::new(9, 0));
            });
            let col = ui.float(0.0, 70.0, |ui| {
                for i in 0..ROWS {
                    ui.leaf(5, L::Px(200.0), L::Px(ROW_H))
                        .pressable(WidgetId(i), Action::new(1, i))
                        .spot(MARK, Action::new(2, i));
                }
            });
            if rows {
                col.rows();
            }
        });
    }
}

fn surface(rows: bool) -> Surface<Recording> {
    let mut s = Surface::new(Recording::new());
    s.frame(&inputs(), &MEASURE, column(rows));
    s
}

#[test]
fn a_row_found_by_its_height_is_the_row_a_walk_finds() {
    let (by_height, walked) = (surface(true), surface(false));
    let mut on = [0usize; 4];
    let mut y = -5.0;
    while y < 70.0 + ROWS as f64 * ROW_H + 10.0 {
        let mut x = -5.0;
        while x < 230.0 {
            let (a, b) = (by_height.hit((x, y)), walked.hit((x, y)));
            assert_eq!(a.map(|h| (h.target, h.action)), b.map(|h| (h.target, h.action)), "at ({x}, {y})");
            on[match a.map(|h| h.action.id) {
                None => 0,
                Some(1) => 1,
                Some(2) => 2,
                _ => 3,
            }] += 1;
            x += 0.5;
        }
        y += 0.5;
    }
    assert!(on.iter().all(|&n| n > 0), "every kind of answer has to have come up: {on:?}");
}

#[test]
fn the_spot_answers_with_its_own_action_and_part() {
    let s = surface(true);
    let row = |i: u32| 70.0 + i as f64 * ROW_H;
    let h = s.hit((MARK.x + 2.0, row(5) + MARK.y + 2.0)).expect("the mark");
    assert_eq!((h.target, h.action), (Target { id: WidgetId(5), item: u32::MAX, part: SPOT }, Action::new(2, 5)));
    let h = s.hit((20.0, row(5) + 10.0)).expect("the row");
    assert_eq!((h.target.part, h.action), (0, Action::new(1, 5)));
    // just past the mark is the row again
    let h = s.hit((MARK.right(), row(5) + MARK.y + 2.0)).expect("the row");
    assert_eq!(h.action, Action::new(1, 5));
}

#[test]
fn moving_onto_a_spot_is_a_change_and_a_press_there_released_on_the_row_is_no_click() {
    let mut s = surface(true);
    let (body, mark) = ((20.0, 70.0 + 10.0), (MARK.x + 2.0, 70.0 + MARK.y + 2.0));
    assert!(s.pointer_move(body));
    assert!(s.pointer_move(mark), "onto the spot of the same row");
    assert!(!s.pointer_move((mark.0 + 1.0, mark.1)));
    s.frame(&inputs(), &MEASURE, |_| panic!("a hover describes nothing"));
    let states: Vec<(bool, bool)> = s
        .output()
        .ops
        .iter()
        .filter_map(|op| if let Op::Leaf { tag: 5, state, .. } = op { Some((state.hovered, state.hovered_spot)) } else { None })
        .take(2)
        .collect();
    assert_eq!(states, [(true, true), (false, false)]);
    let spot = s.output().ops.iter().find_map(|op| if let Op::Leaf { tag: 5, spot, .. } = op { *spot } else { None });
    assert_eq!(spot, Some(Rect::new(MARK.x, 70.0 + MARK.y, MARK.w, MARK.h)), "the paint is told where the spot is");

    s.pointer_down(mark);
    assert_eq!(s.pointer_up(body), None, "pressed on the mark, let go on the row");
    s.pointer_down(mark);
    assert_eq!(s.pointer_up(mark), Some(Action::new(2, 0)));
}

#[test]
fn off_everything_the_pointer_costs_a_rectangle_and_still_puts_out_what_was_lit() {
    let mut s = surface(true);
    assert!(!s.pointer_move((600.0, 500.0)), "nothing lit, nothing under it");
    assert!(s.pointer_move((20.0, 30.0)), "the button");
    assert!(s.pointer_move((600.0, 500.0)), "leaving it is a change though it is off everything");
    assert_eq!(s.interaction.hovered, None);
}
