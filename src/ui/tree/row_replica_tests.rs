//! Two rows of the settings panel, described as a tree, against the
//! same two rows as the panel paints them: every byte of the two
//! frames has to agree.
//!
//! A row is a label, the line under it and a segmented control.  The
//! tree side gets no coordinate from the panel but the row's own band:
//! where the label, the line and each segment go is the tree's layout.
//! One segment is under the pointer in both, found on the tree side by
//! its hit test.
//!
//! Nothing in the app is drawn from a tree yet; the replica lives here.

use super::{FontMeasure, Tone, TreeOut, font_of, metrics, preset};
use crate::paint_parity_tests::{assert_same_pixels_sized, first_difference_in, render_sized};
use crate::settings::Settings;
use crate::ui::components::controls::{self, Measure};
use crate::ui::components::density_kit::at_scale;
use crate::ui::components::settings_modal::{self as sm, Control, Hover, Row, RowSpec, Slot};
use crate::ui::components::settings_paint::{SettingsRender, paint_slot};
use crate::ui::core::ViewPainter;
use crate::ui::strings::t;
use crate::ui::theme::spec::{self, card, space};
use golia_ui_core::{
    AlignCross, Constraints, Edges, Interaction, Layout, Length, Tree, Ui, WidgetId, paint,
};
use marspot_term::language::LangId;
use marspot_term::layout::Rect;

/// The rows compared, and the segment of the second the pointer is on.
const ROWS: [Row; 2] = [Row::Language, Row::DimScale];
const HOVERED: (Row, usize) = (Row::DimScale, 2);

/// Frame size at scale 1.
const FRAME: (u32, u32) = (960, 900);

fn panel(p: &ViewPainter<'_>) -> SettingsRender {
    let settings =
        Settings { appearance_language: Some(marspot_term::language::current_id()), ..Default::default() };
    let margin = spec::px(20.0);
    let rect = Rect { x: margin, y_top: margin, w: spec::px(sm::metric::PANEL_W), h: p.window_h - 2.0 * margin };
    assert!(rect.x + rect.w <= p.window_w, "the frame is narrower than the panel");
    SettingsRender { rect, scroll: 0.0, settings, path: String::new(), hover: Hover::Control(HOVERED.0, HOVERED.1) }
}

/// The panel's cards, and the slots of the rows under comparison.
fn slots(p: &mut ViewPainter<'_>, sp: &SettingsRender) -> (Vec<Slot>, Vec<Slot>) {
    let (mut cards, mut rows) = (Vec::new(), Vec::new());
    let mut m = controls::font_measure(&mut *p.font);
    sm::walk(sp.rect, 0.0, &sp.settings, &mut m, |slot| match slot {
        Slot::Card { .. } => cards.push(slot),
        Slot::Row { row, band, .. } if ROWS.contains(&row) => {
            assert!(band.y_top + band.h <= p.window_h, "{row:?} is below the frame");
            rows.push(slot);
        }
        _ => {}
    });
    assert_eq!(rows.len(), ROWS.len());
    (cards, rows)
}

/// Both sides stand on the cards the panel draws.
fn ground(p: &mut ViewPainter<'_>, sp: &SettingsRender, cards: Vec<Slot>) {
    for card in cards {
        paint_slot(p, sp, card);
    }
}

fn as_the_panel_paints_them(p: &mut ViewPainter<'_>) {
    let sp = panel(p);
    let (cards, rows) = slots(p, &sp);
    ground(p, &sp, cards);
    for row in rows {
        paint_slot(p, &sp, row);
    }
}

/// One row as a tree: a band holding the label and the control, and
/// the line under it, inside the row's padding.
fn describe(ui: &mut Ui<'_>, m: Measure<'_>, row: Row, row_spec: &RowSpec, s: &Settings, band: Rect, gap: f64) {
    let dim = row.disabled_by(s);
    let (fg, sec) = if dim { (Tone::FgFaint, Tone::FgFaint) } else { (Tone::Fg, Tone::FgMuted) };
    let cost = if row == Row::Language { sm::language_cost() } else { row_spec.cost };
    ui.vstack(|ui| {
        ui.hstack(|ui| {
            ui.text(font_of(sm::metric::LABEL), fg.into(), t(row_spec.label));
            ui.spacer();
            let Control::Segmented { options, chosen } = row.control(s) else {
                panic!("{row:?} is not a segmented row")
            };
            let chosen = (chosen < options.len()).then_some(chosen);
            preset::segmented(ui, m, WidgetId(row as u32), 1, options, chosen, dim);
        })
        .height(Length::Pt(sm::metric::ROW_BAND_H))
        .align_cross(AlignCross::Center);
        ui.text(font_of(sm::metric::DESC), sec.into(), t(cost));
    })
    .width(Length::Px(band.w))
    .height(Length::Px(band.h))
    .padding(Edges::xy(Length::Pt(card::ROW_PAD_X), Length::Pt(card::ROW_PAD_Y)))
    .gap(Length::Pt(gap));
}

fn rows_as_trees(p: &mut ViewPainter<'_>, gap: f64) {
    let sp = panel(p);
    let (cards, rows) = slots(p, &sp);
    ground(p, &sp, cards);
    for slot in rows {
        let Slot::Row { row, spec: row_spec, band, control, .. } = slot else { unreachable!() };
        let mut tree = Tree::new();
        {
            let mut m = controls::font_measure(&mut *p.font);
            tree.build(|ui| describe(ui, &mut m, row, row_spec, &sp.settings, band, gap));
        }
        let m = metrics(p.font);
        let mut layout = Layout::new();
        layout.run(
            &tree,
            (band.x, band.y_top),
            Constraints::loose(band.w, band.h),
            &m,
            &FontMeasure::new(&mut *p.font),
        );
        let mut state = Interaction::default();
        if row == HOVERED.0 {
            let Control::Segmented { options, .. } = row.control(&sp.settings) else { unreachable!() };
            let seg =
                controls::segment_rect(control, HOVERED.1, options, &mut controls::font_measure(&mut *p.font));
            state.pointer_move(&tree, &layout, (seg.x + seg.w * 0.5, seg.y_top + seg.h * 0.5));
            let on = state.hovered.expect("the pointer is on a segment");
            assert_eq!((on.id, on.item), (WidgetId(row as u32), HOVERED.1 as u32));
        }
        paint(&tree, &layout, &state, &m, &mut TreeOut::new(p));
    }
}

fn as_a_tree(p: &mut ViewPainter<'_>) {
    rows_as_trees(p, space::LABEL_TO_DESC);
}

/// The same tree with the line under the label half a point lower.
fn as_a_tree_half_a_point_off(p: &mut ViewPainter<'_>) {
    rows_as_trees(p, space::LABEL_TO_DESC + 0.5);
}

fn in_language<T>(lang: LangId, scale: f64, f: impl FnOnce(u32, u32) -> T) -> T {
    at_scale(scale, || {
        crate::settings::set_for_test(Settings { appearance_language: Some(lang), ..Default::default() });
        crate::ui::strings::follow_language();
        let out = f((FRAME.0 as f64 * scale) as u32, (FRAME.1 as f64 * scale) as u32);
        crate::settings::set_for_test(Settings::default());
        crate::ui::strings::follow_language();
        out
    })
}

fn the_tree_draws_the_rows_the_panel_draws(lang: LangId, scale: f64) {
    in_language(lang, scale, |w, h| {
        let name = format!("settings rows, {lang:?} at scale {scale}");
        if assert_same_pixels_sized(&name, as_the_panel_paints_them, as_a_tree, w, h) {
            eprintln!("[tree-replica] {name}: {w} x {h} px, every byte the same");
        }
    });
}

macro_rules! the_tree_draws_the_rows_the_panel_draws {
    ($($test:ident = $lang:expr, $scale:expr;)*) => {$(
        #[test]
        fn $test() {
            the_tree_draws_the_rows_the_panel_draws($lang, $scale);
        }
    )*};
}

the_tree_draws_the_rows_the_panel_draws! {
    tree_replica_of_settings_rows_en_at_1 = LangId::En, 1.0;
    tree_replica_of_settings_rows_en_at_2 = LangId::En, 2.0;
    tree_replica_of_settings_rows_ja_at_1 = LangId::Ja, 1.0;
    tree_replica_of_settings_rows_ja_at_2 = LangId::Ja, 2.0;
    tree_replica_of_settings_rows_zh_at_1 = LangId::ZhHans, 1.0;
    tree_replica_of_settings_rows_zh_at_2 = LangId::ZhHans, 2.0;
}

/// Half a point between the label and its line is one pixel at scale
/// 1: the comparison has to see it, and only in the two lines moved.
#[test]
fn tree_replica_half_a_point_off_is_noticed() {
    in_language(LangId::En, 1.0, |w, h| {
        let (Some(a), Some(b)) =
            (render_sized(as_the_panel_paints_them, w, h), render_sized(as_a_tree_half_a_point_off, w, h))
        else {
            return;
        };
        let ((x, _), n) = first_difference_in(&a, &b, w).expect("a line one pixel lower drew the same bytes");
        assert!(x >= spec::px(20.0) as u32, "the difference is at column {x}, outside the panel");
        assert!(n < 20_000,"{n} pixels moved for two lines of small text; something else differs too");
    });
}
