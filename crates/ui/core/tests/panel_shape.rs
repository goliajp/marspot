//! A form panel, described on the tree, lands where arithmetic says.
//!
//! The shape is the one a settings panel has: a header with a title
//! and a close button, then a scrolling body of a heading, a bordered
//! card of rows, and a footer line.  Each row is a label with a
//! description under it and a control at the right, centred on the
//! label's line; a hairline, inset from the card's sides, sits on the
//! edge between two rows without taking room.
//!
//! The expectations are written the way a hand-rolled layout would
//! compute them, from the same constants, so they check the tree
//! against the arithmetic and not against itself.

use golia_ui_core::{
    Action, AlignCross, Constraints, Edges, FakeMeasure, FontSpec, Ink, Interaction, Layout,
    Length as L, Metrics, NodeId, Op, Recording, Rect, TextFont, Tree, Ui, WidgetId, hit_test,
    paint,
};

const S: f64 = 2.0;
const METRICS: Metrics = Metrics { px_per_pt: S, cell_w: 8.0, cell_h: 16.0, cell_ascent: 12.0 };
const MEASURE: FakeMeasure = FakeMeasure::new(S);

// points, as a design spec would give them
const PANEL_W: f64 = 300.0;
const HEADER_H: f64 = 40.0;
const CLOSE: f64 = 20.0;
const CLOSE_PAD_RIGHT: f64 = 10.0;
const PAD_X: f64 = 18.0;
const PAD_BOTTOM: f64 = 18.0;
const HEADING_TO_CARD: f64 = 8.0;
const ROW_PAD_X: f64 = 14.0;
const ROW_PAD_Y: f64 = 10.0;
const BAND_H: f64 = 28.0;
const LABEL_TO_DESC: f64 = 4.0;
const DESC_DESCENT: f64 = 3.0;
const SEPARATOR_INSET: f64 = 14.0;
const CARD_TO_FOOTER: f64 = 12.0;
const CTL_W: f64 = 36.0;
const CTL_H: f64 = 18.0;
const ROWS: usize = 3;

const TITLE: FontSpec = FontSpec { size_q: 52, weight: 600, features: 0 };
const HEADING: FontSpec = FontSpec { size_q: 44, weight: 600, features: 0 };
const LABEL: FontSpec = FontSpec { size_q: 52, weight: 400, features: 0 };
const DESC: FontSpec = FontSpec { size_q: 44, weight: 400, features: 0 };

const FG: Ink = Ink::Role(1);
const MUTED: Ink = Ink::Role(2);
const CARD_BG: Ink = Ink::Role(3);
const CARD_BORDER: Ink = Ink::Role(4);
const SEPARATOR: Ink = Ink::Role(5);

const FRAME_TAG: u32 = 1;
const CLOSE_TAG: u32 = 2;
const TOGGLE_TAG: u32 = 3;
const HEADER_ID: WidgetId = WidgetId(1);
const CLOSE_ID: WidgetId = WidgetId(2);

fn px(pt: f64) -> f64 {
    pt * S
}

/// Cap height of a font under the fake measurer, device pixels.
fn cap(f: FontSpec) -> f64 {
    f.size_pt() * S * MEASURE.cap_em
}

fn row_h() -> f64 {
    px(ROW_PAD_Y * 2.0 + BAND_H + LABEL_TO_DESC + DESC_DESCENT) + cap(DESC)
}

#[derive(Default)]
struct Ids {
    close: Option<NodeId>,
    title: Option<NodeId>,
    body: Option<NodeId>,
    heading: Option<NodeId>,
    card: Option<NodeId>,
    rows: Vec<NodeId>,
    labels: Vec<NodeId>,
    descs: Vec<NodeId>,
    controls: Vec<NodeId>,
    rules: Vec<NodeId>,
    footer: Option<NodeId>,
}

fn panel(ui: &mut Ui<'_>, ids: &mut Ids) {
    ui.vstack(|ui| {
        ui.hstack(|ui| {
            ids.title = Some(ui.text(TextFont::Ui(TITLE), FG, "Settings").id());
            ui.spacer();
            ids.close = Some(
                ui.leaf(CLOSE_TAG, L::Pt(CLOSE), L::Pt(CLOSE)).widget(CLOSE_ID, Action::new(1, 0)).id(),
            );
        })
        .align_cross(AlignCross::Center)
        .width(L::Pct(1.0))
        .height(L::Pt(HEADER_H))
        .padding(Edges { left: L::Pt(PAD_X), right: L::Pt(CLOSE_PAD_RIGHT), ..Edges::ZERO })
        .widget(HEADER_ID, Action::new(2, 0));

        let body = ui
            .scroll(0, |ui| {
                ids.heading = Some(
                    ui.text(TextFont::Ui(HEADING), FG, "Appearance")
                        .padding(Edges { bottom: L::Pt(HEADING_TO_CARD), ..Edges::ZERO })
                        .id(),
                );
                let card = ui
                    .vstack(|ui| {
                        for k in 0..ROWS {
                            if k > 0 {
                                let rule = ui
                                    .rule(L::Px(1.0), SEPARATOR)
                                    .padding(Edges::xy(L::Pt(SEPARATOR_INSET), L::ZERO))
                                    .id();
                                ids.rules.push(rule);
                            }
                            row(ui, ids, k as u32);
                        }
                    })
                    .width(L::Pct(1.0))
                    .fill(CARD_BG)
                    .border(L::Px(1.0), CARD_BORDER)
                    .radius(L::Pt(8.0))
                    .id();
                ids.card = Some(card);
                ids.footer = Some(
                    ui.text(TextFont::Ui(DESC), MUTED, "~/settings.toml")
                        .padding(Edges { top: L::Pt(CARD_TO_FOOTER), ..Edges::ZERO })
                        .id(),
                );
            })
            .width(L::Pct(1.0))
            .padding(Edges { left: L::Pt(PAD_X), right: L::Pt(PAD_X), bottom: L::Pt(PAD_BOTTOM), ..Edges::ZERO })
            .id();
        ids.body = Some(body);
    })
    .width(L::Pt(PANEL_W))
    .leaf(FRAME_TAG, [0, 0]);
}

fn row(ui: &mut Ui<'_>, ids: &mut Ids, k: u32) {
    let id = ui
        .vstack(|ui| {
            ui.hstack(|ui| {
                ids.labels.push(ui.text(TextFont::Ui(LABEL), FG, "Reduce motion").id());
                ui.spacer();
                let control = ui
                    .leaf(TOGGLE_TAG, L::Pt(CTL_W), L::Pt(CTL_H))
                    .widget(WidgetId(100 + k), Action::new(3, k))
                    .id();
                ids.controls.push(control);
            })
            .align_cross(AlignCross::Center)
            .width(L::Pct(1.0))
            .height(L::Pt(BAND_H));
            ids.descs.push(ui.text(TextFont::Ui(DESC), MUTED, "Costs nothing").id());
        })
        .gap(L::Pt(LABEL_TO_DESC))
        .width(L::Pct(1.0))
        .height(L::Px(row_h()))
        .padding(Edges::xy(L::Pt(ROW_PAD_X), L::Pt(ROW_PAD_Y)))
        .whole()
        .id();
    ids.rows.push(id);
}

struct Built {
    tree: Tree,
    layout: Layout,
    ids: Ids,
}

fn build(window_h: f64) -> Built {
    let mut ids = Ids::default();
    let mut tree = Tree::new();
    tree.build(|ui| panel(ui, &mut ids));
    let mut layout = Layout::new();
    layout.run(&tree, (0.0, 0.0), Constraints::loose(1000.0, window_h), &METRICS, &MEASURE);
    Built { tree, layout, ids }
}

fn close_to(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

fn assert_rect(what: &str, got: Rect, want: Rect) {
    assert!(
        close_to(got.x, want.x) && close_to(got.y, want.y) && close_to(got.w, want.w) && close_to(got.h, want.h),
        "{what}: {got:?}, expected {want:?}"
    );
}

#[test]
fn the_header_holds_the_title_on_its_centre_line_and_the_close_button_at_the_right() {
    let b = build(2000.0);
    let l = &b.layout;
    let frame_w = px(PANEL_W);
    assert_rect(
        "close",
        l.rect(b.ids.close.unwrap()),
        Rect::new(
            frame_w - px(CLOSE_PAD_RIGHT) - px(CLOSE),
            (px(HEADER_H) - px(CLOSE)) * 0.5,
            px(CLOSE),
            px(CLOSE),
        ),
    );
    let title = l.rect(b.ids.title.unwrap());
    assert!(close_to(title.x, px(PAD_X)));
    assert!(
        close_to(title.bottom(), (px(HEADER_H) + cap(TITLE)) * 0.5),
        "the title's baseline is the header's centre line plus half a cap: {title:?}"
    );
}

#[test]
fn rows_controls_and_separators_are_where_a_hand_written_walk_puts_them() {
    let b = build(2000.0);
    let l = &b.layout;
    let content_x = px(PAD_X);
    let content_w = px(PANEL_W) - 2.0 * px(PAD_X);
    let heading = l.inner(b.ids.heading.unwrap());
    assert_rect(
        "heading",
        Rect { w: 0.0, ..heading },
        Rect::new(content_x, px(HEADER_H), 0.0, cap(HEADING)),
    );

    let card_top = px(HEADER_H) + cap(HEADING) + px(HEADING_TO_CARD);
    let card = Rect::new(content_x, card_top, content_w, row_h() * ROWS as f64);
    assert_rect("card", l.rect(b.ids.card.unwrap()), card);

    for k in 0..ROWS {
        let row_top = card.y + row_h() * k as f64;
        let band_top = row_top + px(ROW_PAD_Y);
        assert_rect(
            "row",
            l.rect(b.ids.rows[k]),
            Rect::new(card.x, row_top, card.w, row_h()),
        );
        assert_rect(
            "control",
            l.rect(b.ids.controls[k]),
            Rect::new(
                card.x + card.w - px(ROW_PAD_X) - px(CTL_W),
                band_top + (px(BAND_H) - px(CTL_H)) * 0.5,
                px(CTL_W),
                px(CTL_H),
            ),
        );
        let label = l.rect(b.ids.labels[k]);
        assert!(close_to(label.x, card.x + px(ROW_PAD_X)), "row {k} label x {}", label.x);
        assert!(
            close_to(label.bottom(), band_top + (px(BAND_H) + cap(LABEL)) * 0.5),
            "row {k}: the label's baseline is centred on the band: {label:?}"
        );
        let desc = l.rect(b.ids.descs[k]);
        assert!(
            close_to(desc.bottom(), band_top + px(BAND_H) + px(LABEL_TO_DESC) + cap(DESC)),
            "row {k}: the description's baseline: {desc:?}"
        );
    }
    for (k, rule) in b.ids.rules.iter().enumerate() {
        assert_rect(
            "separator",
            l.rect(*rule),
            Rect::new(
                card.x + px(SEPARATOR_INSET),
                card.y + row_h() * (k + 1) as f64,
                card.w - 2.0 * px(SEPARATOR_INSET),
                1.0,
            ),
        );
    }

    let footer = l.inner(b.ids.footer.unwrap());
    assert!(close_to(footer.bottom(), card.bottom() + px(CARD_TO_FOOTER) + cap(DESC)));
    let content_h = footer.bottom() - px(HEADER_H);
    assert_rect(
        "frame",
        l.rect(NodeId(0)),
        Rect::new(0.0, 0.0, px(PANEL_W), px(HEADER_H) + content_h + px(PAD_BOTTOM)),
    );
    assert_eq!(l.scroll_extent(0).content_h, l.scroll_extent(0).viewport_h, "it all fits: nothing to scroll");
}

#[test]
fn in_a_window_too_short_the_body_scrolls_and_the_panel_stops_at_the_window() {
    let tall = build(2000.0);
    let full_h = tall.layout.rect(NodeId(0)).h;
    let window_h = full_h - 150.0;
    let b = build(window_h);
    let l = &b.layout;
    assert_rect("frame", l.rect(NodeId(0)), Rect::new(0.0, 0.0, px(PANEL_W), window_h));
    let body = l.rect(b.ids.body.unwrap());
    assert!(close_to(body.y, px(HEADER_H)) && close_to(body.bottom(), window_h), "{body:?}");
    let e = l.scroll_extent(0);
    assert!(close_to(e.content_h - e.viewport_h, 150.0), "{e:?}");

    let mut picture = Recording::new();
    let mut state = Interaction::default();
    paint(&b.tree, l, &state, &METRICS, &mut picture);
    let toggles = |r: &Recording| {
        r.ops.iter().filter(|op| matches!(op, Op::Leaf { tag: TOGGLE_TAG, .. })).count()
    };
    let viewport_bottom = window_h - px(PAD_BOTTOM);
    let whole_rows = (0..ROWS).filter(|&k| l.rect(b.ids.rows[k]).bottom() <= viewport_bottom).count();
    assert!(whole_rows < ROWS, "the window was meant to cut a row");
    assert_eq!(toggles(&picture), whole_rows, "a cut row is not drawn at all");
    // the card is drawn down to the viewport's edge and no further
    let card_op = picture.ops.iter().find_map(|op| match op {
        Op::Rounded { rect, fill: Some(CARD_BG), border: Some((_, CARD_BORDER)), .. } => Some(*rect),
        _ => None,
    });
    assert!(close_to(card_op.expect("the card was painted").bottom(), viewport_bottom));

    // the last row's control cannot be hit until it is scrolled into view
    let last = l.rect(b.ids.controls[ROWS - 1]);
    let centre = |r: Rect, dy: f64| (r.x + r.w * 0.5, r.y + r.h * 0.5 - dy);
    assert_eq!(hit_test(&b.tree, l, &state.scroll, state.offset, centre(last, 0.0)), None);
    assert!(state.wheel(l, (px(PANEL_W) * 0.5, px(HEADER_H) + 10.0), 150.0));
    let hit = hit_test(&b.tree, l, &state.scroll, state.offset, centre(last, 150.0));
    assert_eq!(hit.map(|h| h.action), Some(Action::new(3, ROWS as u32 - 1)));
    paint(&b.tree, l, &state, &METRICS, &mut picture);
    assert!(toggles(&picture) >= 1);

    // the header is above the scrolling body and is itself a widget
    let on_header = hit_test(&b.tree, l, &state.scroll, state.offset, (px(PAD_X) + 2.0, 6.0));
    assert_eq!(on_header.map(|h| h.target.id), Some(HEADER_ID));
    let on_close = hit_test(&b.tree, l, &state.scroll, state.offset, centre(l.rect(b.ids.close.unwrap()), 0.0));
    assert_eq!(on_close.map(|h| h.target.id), Some(CLOSE_ID));
}
