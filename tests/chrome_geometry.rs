//! The rects the direct-draw chrome produces, written down.
//!
//! S3-09 moves sixteen components off direct drawing and onto the
//! declarative tree. The point of that move is CPU — H1's single-pass
//! layout and its measurement cache only pay off where the main window
//! draws at 60 fps, and today that window is drawn by code that computes
//! its own rectangles inline.
//!
//! A migration like that is supposed to move nothing on screen, and
//! `tests/layout_geometry.rs` already says why that cannot be checked
//! afterwards: "looks the same is not something a test can check later —
//! so the rects go in here first". It says it about the view tree. The
//! direct-draw side has no such file, so a migration could not be proven
//! to have changed nothing; this is that file for the other side.
//!
//! One component per module, pinned before it is moved. The numbers are
//! what the code produces today, not what anyone thinks it should: a
//! baseline that was reasoned out rather than read off cannot tell you
//! the migration was faithful.

mod context_menu {
    use marspot::ui::components::{ContextMenu, MenuItem};

    /// Logical units at scale 1, so a change to a constant shows up here
    /// as the number it is rather than as a product.
    const SCALE: f64 = 1.0;

    fn items() -> Vec<MenuItem> {
        vec![
            MenuItem::entry("Copy", 1).with_shortcut("⌘C"),
            MenuItem::entry("Paste", 2).with_shortcut("⌘V"),
            MenuItem::divider(),
            MenuItem::entry("Clear scrollback", 3),
            MenuItem::entry("Close pane", 4),
        ]
    }

    fn dump(m: &ContextMenu) -> String {
        let mut s = format!(
            "frame {:.1},{:.1} {:.1}x{:.1}\n",
            m.frame.x, m.frame.y_top, m.frame.w, m.frame.h
        );
        for (i, r) in m.item_rects.iter().enumerate() {
            s += &format!("  [{i}] {:.1},{:.1} {:.1}x{:.1}\n", r.x, r.y_top, r.w, r.h);
        }
        s
    }

    /// Opened with room on every side: the menu sits down-right of the
    /// click by the anchor offset, and every row is the same height
    /// except the divider.
    #[test]
    fn room_on_every_side() {
        let m = ContextMenu::layout(1200.0, 800.0, SCALE, 100.0, 100.0, 0.0, &items());
        assert_eq!(
            dump(&m),
            "frame 102.0,102.0 180.0x124.0\n\
             \x20 [0] 102.0,108.0 180.0x26.0\n\
             \x20 [1] 102.0,134.0 180.0x26.0\n\
             \x20 [2] 102.0,160.0 180.0x8.0\n\
             \x20 [3] 102.0,168.0 180.0x26.0\n\
             \x20 [4] 102.0,194.0 180.0x26.0\n"
        );
    }

    /// Against the right edge it flips to the other side of the click
    /// rather than hanging off the window.
    #[test]
    fn against_the_right_edge_it_flips() {
        let wide = ContextMenu::layout(1200.0, 800.0, SCALE, 100.0, 100.0, 0.0, &items());
        let at_edge = ContextMenu::layout(1200.0, 800.0, SCALE, 1150.0, 100.0, 0.0, &items());
        assert_eq!(at_edge.frame.w, wide.frame.w, "flipping changed the width");
        assert_eq!(
            at_edge.frame.x,
            1150.0 - 2.0 - wide.frame.w,
            "it did not flip to the left of the click"
        );
    }

    /// Against the bottom it flips up, and the flip is measured from the
    /// click rather than from the window.
    #[test]
    fn against_the_bottom_it_flips_up() {
        let m = ContextMenu::layout(1200.0, 800.0, SCALE, 100.0, 780.0, 0.0, &items());
        assert_eq!(m.frame.y_top, 780.0 - 2.0 - m.frame.h);
    }

    /// A title strip at the top is an obstruction: a menu that would have
    /// to flip into it is pinned below it instead.
    #[test]
    fn it_does_not_flip_under_the_title_strip() {
        let obstruction = 40.0;
        let m = ContextMenu::layout(1200.0, 200.0, SCALE, 100.0, 190.0, obstruction, &items());
        assert!(
            m.frame.y_top >= obstruction,
            "the menu starts at {} which is above the obstruction at {obstruction}",
            m.frame.y_top
        );
    }

    /// Width comes from the widest row and is clamped at both ends.
    #[test]
    fn width_is_clamped_at_both_ends() {
        let narrow = ContextMenu::layout(
            1200.0,
            800.0,
            SCALE,
            10.0,
            10.0,
            0.0,
            &[MenuItem::entry("Hi", 1)],
        );
        assert_eq!(narrow.frame.w, 180.0, "a short label fell below the minimum");

        let long: String = std::iter::repeat_n('x', 400).collect();
        let wide = ContextMenu::layout(
            1200.0,
            800.0,
            SCALE,
            10.0,
            10.0,
            0.0,
            &[MenuItem::entry(&long, 1)],
        );
        assert_eq!(wide.frame.w, 360.0, "a long label passed the maximum");
    }

    /// Scale multiplies the geometry rather than being applied somewhere
    /// along the way — the thing a migration is most likely to get wrong,
    /// because the tree carries logical units and the direct-draw code
    /// carries physical ones.
    #[test]
    fn scale_multiplies_everything() {
        let one = ContextMenu::layout(2400.0, 1600.0, 1.0, 100.0, 100.0, 0.0, &items());
        let two = ContextMenu::layout(2400.0, 1600.0, 2.0, 200.0, 200.0, 0.0, &items());
        assert_eq!(two.frame.w, one.frame.w * 2.0);
        assert_eq!(two.frame.h, one.frame.h * 2.0);
        for (a, b) in one.item_rects.iter().zip(two.item_rects.iter()) {
            assert_eq!(b.h, a.h * 2.0, "a row height did not scale");
        }
    }
}

/// Pinned, and deliberately not migrated.
///
/// `ModalFrame::layout` centres a window, clamps it against the screen
/// and stacks three fixed strips inside it. The clamping is placement,
/// which the tree does not do -- it lays children out inside a parent, it
/// does not hold a floating window on screen. What is left for the tree
/// is the stacking, and the body of that stack takes the slack: in this
/// engine only `Spacer` is flexible, so a body that fills would have to be
/// a spacer standing in for content. That is a tree built to satisfy a
/// migration rather than to do anything, so the arithmetic stays and this
/// records why.
///
/// The same goes for the sidebar, whose `row_rect` is `y + i × row_h` and
/// which is called per row per frame -- a layout pass there would cost
/// more than it saves.
///
/// Where the tree pays is content-driven layout: `settings_modal` sizing
/// rows from measured text, `table` sizing columns from their contents.
/// Those are next, and they get pinned the same way first.
mod modal_frame {
    use marspot::ui::components::{ModalFrame, ModalLayoutSpec};

    fn spec(maximized: bool, minimized: bool, with_tab_strip: bool) -> ModalLayoutSpec {
        ModalLayoutSpec {
            default_w: 800.0,
            default_h: 600.0,
            title_bar_h: 28.0,
            tab_strip_h: 30.0,
            maximized,
            max_w_ratio: 0.95,
            max_h_ratio: 0.90,
            minimized,
            with_tab_strip,
            pos_offset: (0.0, 0.0),
            top_obstruction: 0.0,
        }
    }

    fn dump(m: &ModalFrame) -> String {
        let r = |n: &str, r: &marspot_term::layout::Rect| {
            format!("{n} {:.1},{:.1} {:.1}x{:.1}\n", r.x, r.y_top, r.w, r.h)
        };
        r("frame", &m.frame) + &r("title", &m.title_bar) + &r("tabs", &m.tab_strip)
            + &r("body", &m.body)
    }

    /// The in-file tests pin the heights. They do not pin where the tab
    /// strip and the body sit, which is exactly what moving the stacking
    /// onto the tree would change.
    #[test]
    fn the_parts_stack_under_the_title_bar() {
        let m = ModalFrame::layout(1920.0, 1080.0, spec(false, false, true));
        assert_eq!(
            dump(&m),
            "frame 560.0,240.0 800.0x600.0\n\
             title 560.0,240.0 800.0x28.0\n\
             tabs 560.0,268.0 800.0x30.0\n\
             body 560.0,298.0 800.0x542.0\n"
        );
    }

    /// Without a tab strip the body starts where the strip would have.
    #[test]
    fn no_tab_strip_leaves_no_gap() {
        let m = ModalFrame::layout(1920.0, 1080.0, spec(false, false, false));
        assert_eq!(
            dump(&m),
            "frame 560.0,240.0 800.0x600.0\n\
             title 560.0,240.0 800.0x28.0\n\
             tabs 560.0,268.0 800.0x0.0\n\
             body 560.0,268.0 800.0x572.0\n"
        );
    }

    /// Minimized is the title bar and nothing else, and the empty parts
    /// still sit where they would have.
    #[test]
    fn minimized_keeps_the_empty_parts_in_place() {
        let m = ModalFrame::layout(1920.0, 1080.0, spec(false, true, true));
        assert_eq!(
            dump(&m),
            "frame 560.0,240.0 800.0x28.0\n\
             title 560.0,240.0 800.0x28.0\n\
             tabs 560.0,268.0 800.0x0.0\n\
             body 560.0,268.0 800.0x0.0\n"
        );
    }

    /// An offset moves the window and is then clamped, so a drag cannot
    /// put the title bar out of reach.
    #[test]
    fn a_drag_offset_is_clamped_so_the_title_bar_stays_reachable() {
        let mut s = spec(false, false, true);
        s.pos_offset = (100_000.0, 100_000.0);
        let m = ModalFrame::layout(1920.0, 1080.0, s);
        assert_eq!(m.frame.x, 1920.0 - 800.0 * 0.5, "x ran off the right");
        assert_eq!(m.frame.y_top, 1080.0 - 28.0, "the title bar left the window");
    }

    /// A title strip at the top of the window is a floor for the modal.
    #[test]
    fn it_does_not_slide_under_the_title_strip() {
        let mut s = spec(false, false, true);
        s.pos_offset = (0.0, -100_000.0);
        s.top_obstruction = 40.0;
        let m = ModalFrame::layout(1920.0, 1080.0, s);
        assert_eq!(m.frame.y_top, 40.0);
    }
}
