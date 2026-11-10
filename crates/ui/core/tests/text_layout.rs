//! Where text goes, and what is left of it when it does not fit.
//!
//! The measurer is the fake: every character of interface text is half
//! an em wide and a capital is 0.7 em tall, so at 10 pt and two pixels
//! a point a character is 10 px and a cap is 14.

use golia_ui_core::{
    Align, Constraints, FakeMeasure, FontSpec, Ink, Interaction, Layout, Length as L, Metrics,
    NodeId, Op, Recording, Rect, TextFont, Tree, Ui, paint,
};

const METRICS: Metrics = Metrics { px_per_pt: 2.0, cell_w: 8.0, cell_h: 16.0, cell_ascent: 12.0 };
const MEASURE: FakeMeasure = FakeMeasure::new(2.0);
const UI: TextFont = TextFont::Ui(FontSpec { size_q: 40, weight: 400, features: 0 });
const INK: Ink = Ink::Role(0);

struct Laid {
    tree: Tree,
    layout: Layout,
    picture: Recording,
}

fn lay(f: impl FnOnce(&mut Ui<'_>)) -> Laid {
    let mut tree = Tree::new();
    tree.build(f);
    let mut layout = Layout::new();
    layout.run(&tree, (0.0, 0.0), Constraints::loose(400.0, 300.0), &METRICS, &MEASURE);
    let mut picture = Recording::new();
    paint(&tree, &layout, &Interaction::default(), &METRICS, &mut picture);
    Laid { tree, layout, picture }
}

impl Laid {
    /// Every run of text painted: its string, x and baseline.
    fn runs(&self) -> Vec<(&str, f64, f64)> {
        self.picture
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::Text { x, baseline, .. } => Some((self.picture.text(op), *x, *baseline)),
                _ => None,
            })
            .collect()
    }
}

const ROOT: NodeId = NodeId(0);

#[test]
fn interface_text_is_its_width_by_its_cap_height_with_the_baseline_on_the_bottom() {
    let l = lay(|ui| {
        ui.text(UI, INK, "Hello");
    });
    assert_eq!(l.layout.rect(ROOT), Rect::new(0.0, 0.0, 50.0, 14.0));
    let tb = l.layout.text_box(ROOT);
    assert_eq!((tb.w, tb.h, tb.baseline, tb.len, tb.ellipsis), (50.0, 14.0, 14.0, 5, false));
    assert_eq!(l.runs(), [("Hello", 0.0, 14.0)]);
}

/// Centring the cap box in a band puts the baseline at
/// `top + (band + cap) / 2`, which is where a control's own label is
/// drawn — so text beside a control needs no rule of its own.
#[test]
fn a_cap_box_centred_in_a_band_has_its_baseline_where_a_control_puts_its_label() {
    let l = lay(|ui| {
        ui.text(UI, INK, "Hello").height(L::Px(28.0)).width(L::Px(120.0)).align(Align::Leading);
    });
    assert_eq!(l.layout.rect(ROOT), Rect::new(0.0, 0.0, 120.0, 28.0));
    assert_eq!(l.runs(), [("Hello", 0.0, (28.0 + 14.0) * 0.5)]);

    let l = lay(|ui| {
        ui.text(UI, INK, "Hello").height(L::Px(28.0)).width(L::Px(120.0)).align(Align::Trailing);
    });
    assert_eq!(l.runs(), [("Hello", 70.0, 21.0)]);
}

#[test]
fn monospaced_text_is_on_the_cell_grid_and_wide_characters_take_two_cells() {
    let l = lay(|ui| {
        ui.text(TextFont::Mono, INK, "a日b");
    });
    assert_eq!(l.layout.rect(ROOT), Rect::new(0.0, 0.0, 32.0, 16.0));
    assert_eq!(l.runs(), [("a日b", 0.0, 12.0)], "the baseline is the row's ascent down");
}

#[test]
fn text_made_at_run_time_is_copied_once_into_the_trees_buffer() {
    let name = String::from("zsh");
    let l = lay(|ui| {
        ui.vstack(|ui| {
            ui.text_copy(UI, INK, &name);
            ui.text_fmt(UI, INK, format_args!("{}%", 42));
            ui.text(UI, INK, "static");
        });
    });
    assert_eq!(l.tree.text_bytes(), "zsh".len() + "42%".len(), "the static string is not copied");
    let strings: Vec<&str> = l.runs().iter().map(|r| r.0).collect();
    assert_eq!(strings, ["zsh", "42%", "static"]);
}

fn cut(text: &'static str, font: TextFont, max_w: f64) -> Laid {
    lay(|ui| {
        ui.text(font, INK, text).truncate().max_w(L::Px(max_w));
    })
}

#[test]
fn text_that_fits_is_left_alone() {
    let l = cut("abcdef", UI, 60.0);
    let tb = l.layout.text_box(ROOT);
    assert_eq!((tb.len, tb.ellipsis, tb.w), (6, false, 60.0));
    assert_eq!(l.runs(), [("abcdef", 0.0, 14.0)]);
}

#[test]
fn text_that_does_not_fit_is_a_prefix_and_an_ellipsis() {
    // one pixel short of the 60 it needs: four characters and the
    // ellipsis is 50, five and the ellipsis would be 60 again
    let l = cut("abcdef", UI, 59.0);
    let tb = l.layout.text_box(ROOT);
    assert_eq!((tb.len, tb.ellipsis, tb.w, tb.ellipsis_at), (4, true, 50.0, 40.0));
    assert_eq!(l.runs(), [("abcd", 0.0, 14.0), ("…", 40.0, 14.0)]);
    assert_eq!(l.layout.rect(ROOT).w, 50.0, "the box is what is drawn, not what was asked for");

    let l = cut("abcdef", UI, 10.0);
    assert_eq!(l.runs(), [("", 0.0, 14.0), ("…", 0.0, 14.0)], "room for the ellipsis alone");
}

#[test]
fn a_cut_never_lands_inside_a_character() {
    let s = "日本語のテキスト";
    for max_w in [15.0, 25.0, 35.0, 45.0, 55.0, 65.0, 75.0] {
        let l = cut(s, UI, max_w);
        let tb = l.layout.text_box(ROOT);
        assert!(tb.ellipsis, "{max_w}");
        assert!(s.is_char_boundary(tb.len as usize), "{max_w}: cut at byte {}", tb.len);
        assert!(tb.w <= max_w, "{max_w}: drew {} px", tb.w);
    }
    let l = cut(s, UI, 45.0);
    assert_eq!(l.runs(), [("日本語", 0.0, 14.0), ("…", 30.0, 14.0)]);
    let l = cut(s, UI, 80.0);
    assert_eq!(l.runs(), [(s, 0.0, 14.0)], "eight characters are 80");
}

#[test]
fn monospaced_text_is_cut_by_cells_without_asking_for_a_measurement() {
    let l = cut("abcdefgh", TextFont::Mono, 40.0);
    assert_eq!(l.runs(), [("abcd", 0.0, 12.0), ("…", 32.0, 12.0)], "five cells: four and the ellipsis");
    assert_eq!(l.layout.rect(ROOT).w, 40.0);

    // a wide character that would straddle the cut is dropped whole
    let l = cut("日本語日本", TextFont::Mono, 32.0);
    assert_eq!(l.runs(), [("日", 0.0, 12.0), ("…", 16.0, 12.0)]);
    assert_eq!(l.layout.rect(ROOT).w, 24.0);

    let l = cut("abcde", TextFont::Mono, 40.0);
    assert_eq!(l.runs(), [("abcde", 0.0, 12.0)], "five cells fit in five");
}

#[test]
fn text_without_the_flag_is_never_cut() {
    let l = lay(|ui| {
        ui.text(UI, INK, "abcdef").max_w(L::Px(30.0));
    });
    assert_eq!(l.runs(), [("abcdef", 0.0, 14.0)]);
    assert_eq!(l.layout.rect(ROOT).w, 30.0, "its box is held to the limit; its glyphs are not");
}
