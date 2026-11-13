//! Every shared control in every state, in a panel, described as one
//! tree, against the sheet the controls snapshot paints by hand: every
//! byte of the two frames has to agree, at both display scales.
//!
//! The hand-painted sheet shows several controls under the pointer at
//! once.  A surface has one pointer, so the tree side paints through
//! an output that shows as hovered every control in a list; that each
//! of those is what the surface's own hit test finds at its place is
//! asserted separately.

use super::panel::{self, CLOSE_ID};
use super::{FontMeasure, Tone, TreeOut, font_of, metrics, preset};
use crate::paint_parity_tests::{assert_same_pixels_sized, first_difference_in, render_sized};
use crate::ui::components::controls::{self, ButtonKind};
use crate::ui::components::density_kit::at_scale;
use crate::ui::components::panel_chrome;
use crate::ui::components::panel_snapshot_tests::controls_sheet;
use crate::ui::core::ViewPainter;
use crate::ui::strings::Msg;
use crate::ui::theme::spec::{self, card, content, space};
use golia_ui_core::{
    Action, AlignCross, Constraints, Edges, Ink, Interaction, Key, Layout, Length, NO_ITEM, NodeId, PaintOut,
    Rect, Target, TextFont, Tree, Ui, WidgetId, WidgetState, paint,
};

const FRAME: (u32, u32) = (1400, 900);
const LABELS: [Msg; 4] = [Msg::DimOff, Msg::DimLight, Msg::DimNormal, Msg::DimDeep];

const BUTTONS: u32 = 10;
const SEGMENTS: WidgetId = WidgetId(20);
const SEGMENTS_OFF: WidgetId = WidgetId(21);
const TOGGLES: u32 = 30;
const STEPS: u32 = 40;
const DANGERS: u32 = 50;

/// What the hand-painted sheet shows under the pointer.
const HOVERED: [Target; 6] = [
    Target { id: CLOSE_ID, item: NO_ITEM },
    Target { id: WidgetId(BUTTONS + 1), item: NO_ITEM },
    Target { id: WidgetId(BUTTONS + 4), item: NO_ITEM },
    Target { id: SEGMENTS, item: 0 },
    Target { id: WidgetId(STEPS + 1), item: NO_ITEM },
    Target { id: WidgetId(DANGERS + 1), item: NO_ITEM },
];

fn act(n: u32) -> Action {
    Action::new(1, n)
}

/// One group: its heading, then a card one row tall.
fn group(
    ui: &mut Ui<'_>,
    title: &'static str,
    trailing: Option<&'static str>,
    row_h: f64,
    heading_to_card: f64,
    row: impl FnOnce(&mut Ui<'_>),
) {
    ui.hstack(|ui| {
        ui.text(font_of(content::HEADING), Tone::Fg.into(), title);
        ui.spacer();
        if let Some(trailing) = trailing {
            ui.text(font_of(content::HEADING_TRAILING), Tone::FgMuted.into(), trailing);
        }
    })
    .width(Length::Pct(1.0))
    .height(Length::Pt(content::HEADING.cap()))
    .align_cross(AlignCross::End);
    ui.leaf(0, Length::ZERO, Length::Pt(heading_to_card));
    ui.hstack(row)
        .width(Length::Pct(1.0))
        .height(Length::Px(row_h + 2.0 * spec::px(card::ROW_PAD_Y)))
        .padding(Edges::xy(Length::Pt(card::ROW_PAD_X), Length::Pt(card::ROW_PAD_Y)))
        .gap(Length::Pt(space::CONTROL_TO_CONTROL))
        .align_cross(AlignCross::Center)
        .fill(Tone::CardBg.into())
        .border(Length::Px(card::BORDER_PX as f64), Tone::CardBorder.into())
        .radius(Length::Pt(card::RADIUS));
    ui.leaf(0, Length::ZERO, Length::Pt(space::CARD_TO_HEADING));
}

/// Room between two clusters of a row: with the row's own gap either
/// side of it, four gaps.
fn apart(ui: &mut Ui<'_>) {
    ui.leaf(0, Length::Pt(2.0 * space::CONTROL_TO_CONTROL), Length::ZERO);
}

pub(crate) fn sheet(ui: &mut Ui<'_>, fm: &FontMeasure<'_>, frame: (f64, f64), heading_to_card: f64) {
    let mut m = fm.for_controls();
    panel::panel(ui, frame, 0.0, "Controls", Some("updated 11/10 18:00"), |ui| {
        let (_, h) = controls::button_size("Apply", &mut m);
        group(ui, "Buttons", Some("rest · hover · disabled"), h, heading_to_card, |ui| {
            for (n, (label, kind, disabled)) in [
                ("Apply", ButtonKind::Primary, false),
                ("Apply", ButtonKind::Primary, false),
                ("Apply", ButtonKind::Primary, true),
                ("Cancel", ButtonKind::Secondary, false),
                ("Cancel", ButtonKind::Secondary, false),
                ("Cancel", ButtonKind::Secondary, true),
            ]
            .into_iter()
            .enumerate()
            {
                let n = n as u32;
                preset::button(ui, &mut m, WidgetId(BUTTONS + n), act(n), label, kind, disabled);
            }
        });

        let (_, h) = controls::segmented_size(&LABELS, &mut m);
        group(ui, "Segmented control", Some("selected · hover · disabled"), h, heading_to_card, |ui| {
            preset::segmented(ui, &mut m, SEGMENTS, 2, &LABELS, Some(2), false);
            apart(ui);
            preset::segmented(ui, &mut m, SEGMENTS_OFF, 3, &LABELS, Some(2), true);
        });

        let (_, sh) = controls::stepper_size();
        group(ui, "Toggle and stepper", None, sh, heading_to_card, |ui| {
            for (n, (on, disabled)) in [(false, false), (true, false), (false, true), (true, true)].into_iter().enumerate()
            {
                let n = n as u32;
                preset::toggle(ui, WidgetId(TOGGLES + n), act(n), on, disabled);
            }
            apart(ui);
            let step = |n: u32, enabled: bool| preset::Step { id: WidgetId(STEPS + n), action: act(n), enabled };
            preset::stepper(ui, step(0, true), "4", step(1, true));
            apart(ui);
            preset::stepper(ui, step(2, false), "1", step(3, true));
        });

        let (dw, dh) = controls::danger_button_size();
        group(ui, "Status and danger", Some("rest · hover"), dh, heading_to_card, |ui| {
            for (label, severity) in [("OK", 0), ("NEAR", 1), ("LIMIT", 2)] {
                preset::chip(ui, &mut m, label, severity);
            }
            apart(ui);
            preset::danger_button(ui, WidgetId(DANGERS), act(0), dw);
            preset::danger_button(ui, WidgetId(DANGERS + 1), act(1), dw);
        });
    });
}

/// The sheet, described and laid out where the hand-painted one is.
fn laid_out(p: &mut ViewPainter<'_>, heading_to_card: f64) -> (Tree, Layout) {
    let frame = panel_chrome::frame_rect(
        p.window_w,
        p.window_h,
        spec::px(640.0),
        panel_chrome::frame_height(spec::px(268.0)),
        0.0,
    );
    let m = metrics(p.font);
    let fm = FontMeasure::new(&mut *p.font);
    let mut tree = Tree::new();
    tree.build(|ui| sheet(ui, &fm, (frame.w, frame.h), heading_to_card));
    let mut layout = Layout::new();
    layout.run(&tree, (frame.x, frame.y_top), Constraints::loose(frame.w, frame.h), &m, &fm);
    (tree, layout)
}

/// Where the preset drawn for `t` is.
fn rect_of(tree: &Tree, layout: &Layout, t: Target) -> Rect {
    let at = tree
        .nodes()
        .iter()
        .position(|n| n.widget == Some(t.id) && n.item == t.item && n.leaf_tag != 0)
        .unwrap_or_else(|| panic!("no preset is drawn for {t:?}"));
    layout.rect(NodeId(at as u32))
}

/// An output that shows a preset as hovered wherever one of `at` is.
struct ManyPointers<'o, 'p, 'a> {
    out: TreeOut<'p, 'a>,
    at: &'o [Rect],
}

impl PaintOut for ManyPointers<'_, '_, '_> {
    fn reset(&mut self) {
        self.out.reset()
    }
    fn solid_rect(&mut self, rect: Rect, ink: Ink) {
        self.out.solid_rect(rect, ink)
    }
    fn rounded_rect(&mut self, rect: Rect, radius: f64, fill: Option<Ink>, border: Option<(f64, Ink)>) {
        self.out.rounded_rect(rect, radius, fill, border)
    }
    fn text_run(&mut self, x: f64, baseline: f64, text: &str, font: TextFont, ink: Ink) {
        self.out.text_run(x, baseline, text, font, ink)
    }
    fn leaf(&mut self, tag: u32, data: [u32; 2], label: &str, rect: Rect, state: WidgetState) {
        let hovered = state.hovered || (!state.disabled && self.at.contains(&rect));
        self.out.leaf(tag, data, label, rect, WidgetState { hovered, ..state })
    }
    fn focus_ring(&mut self, rect: Rect, radius: f64) {
        self.out.focus_ring(rect, radius)
    }
    fn push_clip(&mut self, rect: Rect) {
        self.out.push_clip(rect)
    }
    fn pop_clip(&mut self) {
        self.out.pop_clip()
    }
}

fn sheet_as_a_tree(p: &mut ViewPainter<'_>, heading_to_card: f64) {
    let (tree, layout) = laid_out(p, heading_to_card);
    let at: Vec<Rect> = HOVERED.iter().map(|t| rect_of(&tree, &layout, *t)).collect();
    // each of them is what one pointer finds there
    for (t, r) in HOVERED.iter().zip(&at) {
        let mut one = Interaction::default();
        one.pointer_move(&tree, &layout, (r.x + r.w * 0.5, r.y + r.h * 0.5));
        assert_eq!(one.hovered, Some(*t), "the pointer on {t:?} is on something else");
    }
    let m = metrics(p.font);
    paint(&tree, &layout, &Interaction::default(), &m, &mut ManyPointers { out: TreeOut::new(p), at: &at });
}

fn as_a_tree(p: &mut ViewPainter<'_>) {
    sheet_as_a_tree(p, space::HEADING_TO_CARD);
}

fn as_a_tree_half_a_point_off(p: &mut ViewPainter<'_>) {
    sheet_as_a_tree(p, space::HEADING_TO_CARD + 0.5);
}

fn frame_at(scale: f64) -> (u32, u32) {
    ((FRAME.0 as f64 * scale) as u32, (FRAME.1 as f64 * scale) as u32)
}

fn the_tree_draws_the_sheet(scale: f64) {
    at_scale(scale, || {
        let (w, h) = frame_at(scale);
        let name = format!("controls sheet at scale {scale}");
        if assert_same_pixels_sized(&name, controls_sheet, as_a_tree, w, h) {
            eprintln!("[preset-parity] {name}: {w} x {h} px, every byte the same");
        }
    });
}

#[test]
fn presets_in_a_tree_are_the_controls_sheet_at_1() {
    the_tree_draws_the_sheet(1.0);
}

#[test]
fn presets_in_a_tree_are_the_controls_sheet_at_2() {
    the_tree_draws_the_sheet(2.0);
}

/// Every card half a point lower under its heading: the comparison
/// has to see it.
#[test]
fn a_sheet_half_a_point_off_is_noticed() {
    at_scale(1.0, || {
        let (w, h) = frame_at(1.0);
        let (Some(a), Some(b)) = (render_sized(controls_sheet, w, h), render_sized(as_a_tree_half_a_point_off, w, h))
        else {
            return;
        };
        let (_, n) = first_difference_in(&a, &b, w).expect("cards one pixel lower drew the same bytes");
        assert!(n > 100, "{n} pixels differ; four cards moved");
    });
}

/// The sheet with the keyboard focus moved onto its first button.
fn focused_by_tab(p: &mut ViewPainter<'_>) {
    let (tree, layout) = laid_out(p, space::HEADING_TO_CARD);
    let mut state = Interaction::default();
    state.key(&tree, &layout, Key::Tab);
    assert_eq!(state.focused.map(|t| t.id), Some(CLOSE_ID), "the close button is the first stop");
    state.key(&tree, &layout, Key::Tab);
    assert_eq!(state.focused.map(|t| t.id), Some(WidgetId(BUTTONS)));
    let m = metrics(p.font);
    paint(&tree, &layout, &state, &m, &mut TreeOut::new(p));
}

/// The same, after the pointer pressed that button and let go.
fn focused_by_press(p: &mut ViewPainter<'_>) {
    let (tree, layout) = laid_out(p, space::HEADING_TO_CARD);
    let r = rect_of(&tree, &layout, Target { id: WidgetId(BUTTONS), item: NO_ITEM });
    let at = (r.x + r.w * 0.5, r.y + r.h * 0.5);
    let mut state = Interaction::default();
    state.key(&tree, &layout, Key::Tab);
    state.pointer_down(&tree, &layout, at);
    assert_eq!(state.pointer_up(&tree, &layout, at), Some(act(0)));
    assert_eq!(state.focused.map(|t| t.id), Some(WidgetId(BUTTONS)), "the press took the focus");
    let m = metrics(p.font);
    paint(&tree, &layout, &state, &m, &mut TreeOut::new(p));
}

fn at_rest(p: &mut ViewPainter<'_>) {
    let (tree, layout) = laid_out(p, space::HEADING_TO_CARD);
    let m = metrics(p.font);
    paint(&tree, &layout, &Interaction::default(), &m, &mut TreeOut::new(p));
}

/// The focus is shown after a key and only then: a ring round the
/// focused button and nothing else changed; after a press, the frame
/// is the one with no focus at all.
fn the_ring_follows_the_keyboard(scale: f64) {
    at_scale(scale, || {
        let (w, h) = frame_at(scale);
        let (Some(rest), Some(tab), Some(press)) =
            (render_sized(at_rest, w, h), render_sized(focused_by_tab, w, h), render_sized(focused_by_press, w, h))
        else {
            return;
        };
        assert_eq!(first_difference_in(&rest, &press, w), None, "a press left the focus showing");
        let (_, n) = first_difference_in(&rest, &tab, w).expect("Tab drew no ring");
        // a ring one point wide round a button 88 by 28 points
        let (bw, bh) = (spec::px(88.0), spec::px(28.0));
        let most = 2.0 * (bw + bh) * spec::px(4.0);
        assert!((n as f64) < most, "{n} pixels changed, more than a ring round one button ({most})");
        let mut differ = rest.chunks_exact(4).zip(tab.chunks_exact(4)).enumerate().filter(|(_, (a, b))| a != b);
        let (i, _) = differ.next().unwrap();
        let (x, y) = (i as u32 % w, i as u32 / w);
        eprintln!("[focus-ring] scale {scale}: {n} pixels, the first at ({x}, {y})");
    });
}

#[test]
fn the_focus_ring_follows_the_keyboard_at_1() {
    the_ring_follows_the_keyboard(1.0);
}

#[test]
fn the_focus_ring_follows_the_keyboard_at_2() {
    the_ring_follows_the_keyboard(2.0);
}

/// The sheet with the focus ring showing, for eyes.
#[test]
fn panel_snapshot_controls_focus() {
    crate::ui::components::panel_snapshot_tests::snapshot("controls_focus", FRAME.0, FRAME.1, |_, _| {
        *crate::frame_build::frame::TEST_OVERLAY.lock().unwrap() = Some(focused_by_tab);
    });
}

#[test]
fn panel_snapshot_controls_focus_2x() {
    at_scale(2.0, panel_snapshot_controls_focus);
}
