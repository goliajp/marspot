//! The geometry a tree lays out to, written down.
//!
//! Layout is about to change how many passes it makes over a tree.
//! That change is supposed to move no pixel, and "looks the same" is
//! not something a test can check later — so the rects go in here
//! first, as text, for a set of trees that between them exercise
//! every rule the stacks have: intrinsic sizing, fixed frames,
//! percentages, padding, spacers taking the slack, cross-axis
//! stretch, and nesting.
//!
//! A failure here is either a real geometry change or an expectation
//! that was updated for a good reason.  Both are worth stopping for.

use marspot::ui::core::Length as L;
use marspot::ui::view::{
    AlignCross, Constraints, Edges, FrameSpec, LaidOut, LayoutCtx, MockFontMetrics, Text,
    UiSize, View, hstack, layout_view, spacer, vstack,
};

fn ctx<'a>(fonts: &'a MockFontMetrics) -> LayoutCtx<'a> {
    LayoutCtx { scale: 2.0, cell_w_phys: 16.0, cell_h_phys: 32.0, ascent_phys: 24.0, fonts }
}

/// One line per node: indent for depth, the node's kind, its rect.
fn dump(laid: &LaidOut) -> String {
    fn kind(v: &View) -> &'static str {
        match v {
            View::Text(_) => "text",
            View::Spacer { .. } => "spacer",
            View::VStack { .. } => "vstack",
            View::HStack { .. } => "hstack",
            View::ZStack { .. } => "zstack",
            _ => "other",
        }
    }
    fn walk(n: &LaidOut, depth: usize, out: &mut String) {
        out.push_str(&format!(
            "{:indent$}{} {:.1} {:.1} {:.1} {:.1}\n",
            "",
            kind(&n.view),
            n.rect.x,
            n.rect.y,
            n.rect.w,
            n.rect.h,
            indent = depth * 2,
        ));
        for c in &n.children {
            walk(c, depth + 1, out);
        }
    }
    let mut out = String::new();
    walk(laid, 0, &mut out);
    out
}

fn label(s: &str) -> View {
    Text::new(s).ui_size(UiSize::Body).build()
}

fn check(name: &str, view: View, c: Constraints, expected: &str) {
    let fonts = MockFontMetrics { cell_w_phys: 16.0, cell_h_phys: 32.0 };
    let laid = layout_view(&view, ctx(&fonts), (0.0, 0.0), c);
    let got = dump(&laid);
    assert_eq!(
        got.trim_end(),
        expected.trim_end(),
        "{name}: geometry moved\n--- got ---\n{got}--- expected ---\n{expected}"
    );
}

#[test]
fn a_column_stacks_its_children_and_takes_their_width() {
    check(
        "column",
        vstack(vec![label("ab"), label("cdef")]),
        Constraints::loose(800.0, 600.0),
        "\
vstack 0.0 0.0 32.0 51.2
  text 0.0 0.0 16.0 25.6
  text 0.0 25.6 32.0 25.6",
    );
}

#[test]
fn a_spacer_takes_the_slack_and_pushes_the_rest_to_the_end() {
    check(
        "spacer",
        hstack(vec![label("ab"), spacer(), label("cd")])
            .frame(FrameSpec { width: Some(L::Pt(200.0)), ..Default::default() }),
        Constraints::loose(800.0, 600.0),
        "\
other 0.0 0.0 400.0 25.6
  hstack 0.0 0.0 400.0 25.6
    text 0.0 0.0 16.0 25.6
    spacer 16.0 0.0 368.0 0.0
    text 384.0 0.0 16.0 25.6",
    );
}

#[test]
fn padding_insets_the_child_and_grows_the_parent() {
    check(
        "padding",
        vstack(vec![label("ab").padding(Edges::xy(L::Pt(8.0), L::Pt(4.0)))]),
        Constraints::loose(800.0, 600.0),
        "\
vstack 0.0 0.0 48.0 41.6
  other 0.0 0.0 48.0 41.6
    text 16.0 8.0 16.0 25.6",
    );
}

#[test]
fn a_fixed_frame_overrides_the_intrinsic_size() {
    check(
        "frame",
        vstack(vec![
            label("ab").frame(FrameSpec {
                width: Some(L::Pt(50.0)),
                height: Some(L::Pt(20.0)),
                ..Default::default()
            }),
            label("cd"),
        ]),
        Constraints::loose(800.0, 600.0),
        "\
vstack 0.0 0.0 100.0 65.6
  other 0.0 0.0 100.0 40.0
    text 0.0 0.0 100.0 40.0
  text 0.0 40.0 16.0 25.6",
    );
}

#[test]
fn cross_axis_stretch_widens_every_child_to_the_widest() {
    check(
        "stretch",
        vstack(vec![label("ab"), label("cdef")]).align_cross(AlignCross::Stretch),
        Constraints::loose(800.0, 600.0),
        "\
vstack 0.0 0.0 800.0 51.2
  text 0.0 0.0 800.0 25.6
  text 0.0 25.6 800.0 25.6",
    );
}

#[test]
fn nesting_places_a_grandchild_by_summing_the_offsets() {
    check(
        "nested",
        vstack(vec![
            label("ab"),
            hstack(vec![label("cd"), label("ef")])
                .padding(Edges::xy(L::Pt(4.0), L::Pt(2.0))),
        ]),
        Constraints::loose(800.0, 600.0),
        "\
vstack 0.0 0.0 48.0 59.2
  text 0.0 0.0 16.0 25.6
  other 0.0 25.6 48.0 33.6
    hstack 8.0 29.6 32.0 25.6
      text 8.0 29.6 16.0 25.6
      text 24.0 29.6 16.0 25.6",
    );
}

/// The guard has to be able to fail.
///
/// Six expectations that all pass read the same whether the comparison
/// is real or the dump is empty.  This one feeds `check` a geometry
/// that is not the one the tree lays out to, and requires it to say so.
#[test]
fn the_guard_notices_a_moved_rect() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(|| {
        check(
            "control",
            vstack(vec![label("ab")]),
            Constraints::loose(800.0, 600.0),
            "vstack 0.0 0.0 999.0 999.0\n  text 0.0 0.0 1.0 1.0",
        );
    });
    std::panic::set_hook(prev);
    assert!(r.is_err(), "a wrong expectation has to fail, or the other six mean nothing");
}
