//! The geometry a tree lays out to, written down.
//!
//! The first six cases are the ones the previous tree was held to,
//! with the rectangles it produced, number for number: intrinsic
//! sizing, fixed frames, percentages, padding, spacers taking the
//! slack, cross-axis stretch, and nesting.  That tree wrapped a node in
//! another one to pad or size it; here a node carries its own padding
//! and size, so where the old dump had a wrapper line, this one prints
//! the node's border box under the same name and its content box
//! beneath.
//!
//! The measurer is set so a line of text is as tall as the old mock
//! made it (25.6 px at 8 pt), which keeps the old numbers comparable.

use golia_ui_core::{
    Align, AlignCross, At, Constraints, Edges, FakeMeasure, FontSpec, Ink, Layout, Length as L,
    Metrics, NodeId, NodeKind, Rect, Style, TextFont, Tree, Ui,
};

const METRICS: Metrics = Metrics { px_per_pt: 2.0, cell_w: 16.0, cell_h: 32.0, cell_ascent: 24.0 };
const MEASURE: FakeMeasure =
    FakeMeasure { px_per_pt: 2.0, advance_em: 0.5, cap_em: 1.6, line_em: 1.6, ascent_em: 1.2 };
const BODY: TextFont = TextFont::Ui(FontSpec { size_q: 32, weight: 400, features: 0 });
const INK: Ink = Ink::Role(0);

fn label<'u, 't>(ui: &'u mut Ui<'t>, s: &'static str) -> At<'u, 't> {
    ui.text(BODY, INK, s)
}

fn lay(c: Constraints, f: impl FnOnce(&mut Ui<'_>)) -> (Tree, Layout) {
    let mut tree = Tree::new();
    tree.build(f);
    let mut layout = Layout::new();
    layout.run(&tree, (0.0, 0.0), c, &METRICS, &MEASURE);
    (tree, layout)
}

/// One line per box: indent for depth, the kind, the rect.
fn dump(tree: &Tree, layout: &Layout) -> String {
    fn line(out: &mut String, depth: usize, name: &str, r: Rect) {
        out.push_str(&format!(
            "{:indent$}{name} {:.1} {:.1} {:.1} {:.1}\n",
            "",
            r.x,
            r.y,
            r.w,
            r.h,
            indent = depth * 2
        ));
    }
    let mut out = String::new();
    // a parent comes before its children, so its depth is known by then
    let mut depths = vec![0usize; tree.len()];
    for (i, n) in tree.nodes().iter().enumerate() {
        let id = NodeId(i as u32);
        let name = match n.kind {
            NodeKind::Text => "text",
            NodeKind::Spacer => "spacer",
            NodeKind::VStack => "vstack",
            NodeKind::HStack => "hstack",
            NodeKind::ZStack => "zstack",
            _ => "other",
        };
        let mut depth = tree.parent(id).map_or(0, |p| depths[p.0 as usize] + 1);
        if n.style != Style::DEFAULT {
            line(&mut out, depth, "other", layout.rect(id));
            depth += 1;
            line(&mut out, depth, name, layout.inner(id));
        } else {
            line(&mut out, depth, name, layout.rect(id));
        }
        depths[i] = depth;
    }
    out
}

fn check(name: &str, c: Constraints, f: impl FnOnce(&mut Ui<'_>), expected: &str) {
    let (tree, layout) = lay(c, f);
    let got = dump(&tree, &layout);
    assert_eq!(
        got.trim_end(),
        expected.trim_end(),
        "{name}: geometry moved\n--- got ---\n{got}--- expected ---\n{expected}"
    );
}

fn loose() -> Constraints {
    Constraints::loose(800.0, 600.0)
}

#[test]
fn a_column_stacks_its_children_and_takes_their_width() {
    check(
        "column",
        loose(),
        |ui| {
            ui.vstack(|ui| {
                label(ui, "ab");
                label(ui, "cdef");
            });
        },
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
        loose(),
        |ui| {
            ui.hstack(|ui| {
                label(ui, "ab");
                ui.spacer();
                label(ui, "cd");
            })
            .width(L::Pt(200.0));
        },
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
        loose(),
        |ui| {
            ui.vstack(|ui| {
                label(ui, "ab").padding(Edges::xy(L::Pt(8.0), L::Pt(4.0)));
            });
        },
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
        loose(),
        |ui| {
            ui.vstack(|ui| {
                label(ui, "ab").width(L::Pt(50.0)).height(L::Pt(20.0));
                label(ui, "cd");
            });
        },
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
        loose(),
        |ui| {
            ui.vstack(|ui| {
                label(ui, "ab");
                label(ui, "cdef");
            })
            .align_cross(AlignCross::Stretch);
        },
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
        loose(),
        |ui| {
            ui.vstack(|ui| {
                label(ui, "ab");
                ui.hstack(|ui| {
                    label(ui, "cd");
                    label(ui, "ef");
                })
                .padding(Edges::xy(L::Pt(4.0), L::Pt(2.0)));
            });
        },
        "\
vstack 0.0 0.0 48.0 59.2
  text 0.0 0.0 16.0 25.6
  other 0.0 25.6 48.0 33.6
    hstack 8.0 29.6 32.0 25.6
      text 8.0 29.6 16.0 25.6
      text 24.0 29.6 16.0 25.6",
    );
}

/// Six expectations that all pass read the same whether the comparison
/// is real or the dump is empty.  This one is wrong on purpose.
#[test]
fn the_guard_notices_a_moved_rect() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(|| {
        check(
            "control",
            loose(),
            |ui| {
                ui.vstack(|ui| {
                    label(ui, "ab");
                });
            },
            "vstack 0.0 0.0 999.0 999.0\n  text 0.0 0.0 1.0 1.0",
        );
    });
    std::panic::set_hook(prev);
    assert!(r.is_err(), "a wrong expectation has to fail, or the other six mean nothing");
}

#[test]
fn a_rule_sits_on_the_edge_between_two_rows_and_takes_no_room() {
    let mut rule = NodeId(0);
    let (_, layout) = lay(loose(), |ui| {
        ui.vstack(|ui| {
            label(ui, "ab").height(L::Px(40.0)).width(L::Px(300.0));
            rule = ui.rule(L::Px(1.0), INK).id();
            label(ui, "cd").height(L::Px(40.0));
        });
    });
    assert_eq!(layout.rect(rule), Rect::new(0.0, 40.0, 300.0, 1.0));
    // the column is two rows tall, not two rows and a rule
    assert_eq!(layout.rect(NodeId(0)), Rect::new(0.0, 0.0, 300.0, 80.0));
    assert_eq!(layout.rect(NodeId(3)).y, 40.0);
}

#[test]
fn a_growing_child_takes_what_the_others_left() {
    let (mut grown, mut fixed) = (NodeId(0), NodeId(0));
    let (_, layout) = lay(loose(), |ui| {
        ui.hstack(|ui| {
            grown = label(ui, "a long label that will not fit").grow(1).truncate().id();
            fixed = ui.leaf(0, L::Px(100.0), L::Px(30.0)).id();
        })
        .width(L::Px(260.0));
    });
    assert_eq!(layout.rect(grown), Rect::new(0.0, 0.0, 160.0, 25.6));
    assert_eq!(layout.rect(fixed), Rect::new(160.0, 0.0, 100.0, 30.0));
}

/// The previous tree moved only the child's own rect when a z-stack
/// anchored it, and left the child's descendants where they were.
#[test]
fn a_zstack_anchors_a_child_together_with_what_is_inside_it() {
    let (mut inner_text, mut small) = (NodeId(0), NodeId(0));
    let (_, layout) = lay(loose(), |ui| {
        ui.zstack(Align::Center, |ui| {
            ui.leaf(0, L::Px(200.0), L::Px(100.0));
            small = ui
                .vstack(|ui| {
                    inner_text = label(ui, "ab").id();
                })
                .id();
        });
    });
    assert_eq!(layout.rect(small), Rect::new(92.0, (100.0 - 25.6) * 0.5, 16.0, 25.6));
    assert_eq!(layout.rect(inner_text), layout.rect(small));
}

#[test]
fn a_scroll_region_is_as_tall_as_it_is_allowed_and_knows_how_much_it_holds() {
    let mut region = NodeId(0);
    let (_, layout) = lay(Constraints::loose(400.0, 100.0), |ui| {
        region = ui
            .scroll(1, |ui| {
                for _ in 0..10 {
                    ui.leaf(0, L::Px(300.0), L::Px(30.0));
                }
            })
            .id();
    });
    assert_eq!(layout.rect(region), Rect::new(0.0, 0.0, 300.0, 100.0));
    let e = layout.scroll_extent(1);
    assert_eq!((e.node, e.content_h, e.viewport_h), (Some(region), 300.0, 100.0));
    // content keeps its unscrolled place
    assert_eq!(layout.rect(NodeId(10)), Rect::new(0.0, 270.0, 300.0, 30.0));
}

/// The previous tree aligned children within the extent they would
/// have taken and left a taller frame's slack underneath.  A row held
/// to a height is where a label and a control are centred, so the
/// alignment is within the height the row was given.
#[test]
fn a_row_held_to_a_height_centres_its_children_in_that_height() {
    let (mut text, mut control) = (NodeId(0), NodeId(0));
    let (_, layout) = lay(loose(), |ui| {
        ui.hstack(|ui| {
            text = label(ui, "ab").id();
            ui.spacer();
            control = ui.leaf(0, L::Px(60.0), L::Px(30.0)).id();
        })
        .align_cross(AlignCross::Center)
        .width(L::Px(300.0))
        .height(L::Px(56.0));
    });
    assert_eq!(layout.rect(text), Rect::new(0.0, (56.0 - 25.6) * 0.5, 16.0, 25.6));
    assert_eq!(layout.rect(control), Rect::new(240.0, 13.0, 60.0, 30.0));
}

#[test]
fn a_column_too_tall_for_its_space_shortens_its_scroll_region_and_nothing_else() {
    let (mut head, mut body, mut foot) = (NodeId(0), NodeId(0), NodeId(0));
    let (_, layout) = lay(Constraints::loose(400.0, 200.0), |ui| {
        ui.vstack(|ui| {
            head = ui.leaf(0, L::Px(100.0), L::Px(40.0)).id();
            body = ui
                .scroll(0, |ui| {
                    ui.leaf(0, L::Px(100.0), L::Px(500.0));
                })
                .id();
            foot = ui.leaf(0, L::Px(100.0), L::Px(20.0)).id();
        });
    });
    assert_eq!(layout.rect(head), Rect::new(0.0, 0.0, 100.0, 40.0));
    assert_eq!(layout.rect(body), Rect::new(0.0, 40.0, 100.0, 140.0));
    assert_eq!(layout.rect(foot), Rect::new(0.0, 180.0, 100.0, 20.0));
    assert_eq!(layout.rect(NodeId(0)).h, 200.0);
    let e = layout.scroll_extent(0);
    assert_eq!((e.content_h, e.viewport_h), (500.0, 140.0));
}
