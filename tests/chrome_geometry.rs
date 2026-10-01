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

/// The settings panel, which is the first component whose layout is
/// driven by measured text rather than by constants alone.
///
/// Its own tests are relations — a control is clickable where it is
/// drawn, nothing overflows its card, the real font fits. All of them
/// survive a panel moved three pixels down, which is exactly the
/// failure a migration makes: the context menu's six relational tests
/// all passed on a half-migrated tree that took only the frame from it.
/// So the rects go here as numbers.
mod settings_modal {
    use marspot::settings::Settings;
    use marspot::ui::components::settings_modal::{
        Slot, hit_test, max_scroll, panel_rect, walk, walk_visible,
    };
    use marspot_term::layout::Rect;

    /// `px_per_pt` reads a process-global scale, so a pin on absolute
    /// px is only stable while this module owns it. Held for each
    /// test's whole body, not just while setting it.
    static SCALE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn at_scale_1<R>(f: impl FnOnce() -> R) -> R {
        let _g = SCALE.lock().unwrap_or_else(|e| e.into_inner());
        marspot::ui::set_chrome_scale(1.0);
        f()
    }

    /// A measure that does not need a font: width proportional to the
    /// character count, with a bold weight costing a little more. The
    /// point of a pin is that the same input gives the same rects, so
    /// what matters is that it is deterministic and that the migrated
    /// code is handed the same one.
    fn measure() -> impl FnMut(&str, f64, u16) -> f64 {
        |s: &str, pt: f64, w: u16| {
            let bold = if w >= 600 { 1.05 } else { 1.0 };
            s.chars().count() as f64 * pt * 0.5 * bold * 2.0
        }
    }

    fn r(x: &Rect) -> String {
        format!("{:.1},{:.1} {:.1}x{:.1}", x.x, x.y_top, x.w, x.h)
    }

    fn dump(w: f64, h: f64, s: &Settings) -> String {
        let mut m = measure();
        let rect = panel_rect(w, h, s, &mut m, 30.0);
        let mut out = format!("panel {}\n", r(&rect));
        let mut m = measure();
        walk(rect, 0.0, s, &mut m, |slot| match slot {
            Slot::Title { baseline } => out += &format!("title base {baseline:.1}\n"),
            Slot::Group { heading, baseline } => {
                out += &format!("group {heading:?} base {baseline:.1}\n")
            }
            Slot::Card { rect } => out += &format!("card {}\n", r(&rect)),
            Slot::Row { row, band, label_baseline, desc_baseline, control, .. } => {
                out += &format!(
                    "row {row:?} band {} label {label_baseline:.1} desc {desc_baseline:.1} ctl {}\n",
                    r(&band),
                    r(&control)
                )
            }
            Slot::Separator { rect } => out += &format!("sep {}\n", r(&rect)),
            Slot::Footer { baseline } => out += &format!("footer base {baseline:.1}\n"),
        });
        out
    }

    /// Room on every side: the panel is its content's size, centred.
    #[test]
    fn a_window_with_room_gets_the_panel_at_its_natural_size() {
        at_scale_1(|| {
            assert_eq!(
                dump(1600.0, 1200.0, &Settings::default()),
                "panel 360.0,152.4 880.0x895.2\n\
                 title base 198.4\n\
                 group \"Idle reclamation\" base 238.4\n\
                 card 396.0,252.4 808.0x245.7\n\
                 row ReclaimEnabled band 396.0,252.4 808.0x81.9 label 289.9 desc 315.4 ctl 1138.0,272.4 42.0x24.0\n\
                 sep 420.0,334.3 784.0x1.0\n\
                 row ReclaimIdleMinutes band 396.0,334.3 808.0x81.9 label 371.8 desc 397.3 ctl 897.0,350.3 283.0x32.0\n\
                 sep 420.0,416.2 784.0x1.0\n\
                 row ReclaimPrefetch band 396.0,416.2 808.0x81.9 label 453.7 desc 479.2 ctl 1138.0,436.2 42.0x24.0\n\
                 group \"Appearance\" base 534.1\n\
                 card 396.0,548.1 808.0x163.8\n\
                 row DimScale band 396.0,548.1 808.0x81.9 label 585.6 desc 611.1 ctl 915.2,564.1 264.8x32.0\n\
                 sep 420.0,630.0 784.0x1.0\n\
                 row CircledWide band 396.0,630.0 808.0x81.9 label 667.5 desc 693.0 ctl 1138.0,650.0 42.0x24.0\n\
                 group \"Claude Code\" base 747.9\n\
                 card 396.0,761.9 808.0x81.9\n\
                 row CcStatuslineHook band 396.0,761.9 808.0x81.9 label 799.4 desc 824.9 ctl 1138.0,781.9 42.0x24.0\n\
                 group \"Scrolling\" base 879.8\n\
                 card 396.0,893.8 808.0x81.9\n\
                 row ScrollFactor band 396.0,893.8 808.0x81.9 label 931.3 desc 956.8 ctl 902.0,909.8 278.0x32.0\n\
                 footer base 1016.7\n"
            );
        });
    }

    /// Narrow: the width clamps to 94% of the window and everything
    /// inside follows it -- the cards narrow, the separators narrow,
    /// and the right-aligned controls move left by the same amount.
    /// The vertical rhythm does not move, because nothing in it
    /// depends on the width.
    #[test]
    fn a_narrow_window_clamps_the_width_and_the_contents_follow() {
        at_scale_1(|| {
            assert_eq!(
                dump(800.0, 1200.0, &Settings::default()),
                "panel 24.0,152.4 752.0x895.2\n\
                 title base 198.4\n\
                 group \"Idle reclamation\" base 238.4\n\
                 card 60.0,252.4 680.0x245.7\n\
                 row ReclaimEnabled band 60.0,252.4 680.0x81.9 label 289.9 desc 315.4 ctl 674.0,272.4 42.0x24.0\n\
                 sep 84.0,334.3 656.0x1.0\n\
                 row ReclaimIdleMinutes band 60.0,334.3 680.0x81.9 label 371.8 desc 397.3 ctl 433.0,350.3 283.0x32.0\n\
                 sep 84.0,416.2 656.0x1.0\n\
                 row ReclaimPrefetch band 60.0,416.2 680.0x81.9 label 453.7 desc 479.2 ctl 674.0,436.2 42.0x24.0\n\
                 group \"Appearance\" base 534.1\n\
                 card 60.0,548.1 680.0x163.8\n\
                 row DimScale band 60.0,548.1 680.0x81.9 label 585.6 desc 611.1 ctl 451.2,564.1 264.8x32.0\n\
                 sep 84.0,630.0 656.0x1.0\n\
                 row CircledWide band 60.0,630.0 680.0x81.9 label 667.5 desc 693.0 ctl 674.0,650.0 42.0x24.0\n\
                 group \"Claude Code\" base 747.9\n\
                 card 60.0,761.9 680.0x81.9\n\
                 row CcStatuslineHook band 60.0,761.9 680.0x81.9 label 799.4 desc 824.9 ctl 674.0,781.9 42.0x24.0\n\
                 group \"Scrolling\" base 879.8\n\
                 card 60.0,893.8 680.0x81.9\n\
                 row ScrollFactor band 60.0,893.8 680.0x81.9 label 931.3 desc 956.8 ctl 438.0,909.8 278.0x32.0\n\
                 footer base 1016.7\n"
            );
        });
    }

    /// A window too short for the panel used to spill: `panel_rect`
    /// clamped the height to 94% of the window, `walk` never saw the
    /// clamp and laid out from the top regardless, and the paint path
    /// has no scissor -- so the last groups drew over the terminal and
    /// off the bottom, still clickable where nobody could reach them.
    ///
    /// The panel's own `the_panel_fits_the_window_it_is_centred_in`
    /// missed it by asserting the *rect* fits the window and never
    /// that the *content* fits the rect. This is that assertion.
    #[test]
    fn a_short_window_keeps_everything_it_draws_inside_the_panel() {
        at_scale_1(|| {
            let s = Settings::default();
            for scroll in [0.0f64, 100.0, 1000.0] {
                let mut m = measure();
                let rect = panel_rect(1600.0, 700.0, &s, &mut m, 30.0);
                let scroll = scroll.min(max_scroll(rect, &s, &mut measure()));
                let mut m = measure();
                let bottom = rect.y_top + rect.h;
                let mut drew = 0usize;
                walk_visible(rect, scroll, &s, &mut m, |slot| {
                    drew += 1;
                    let r = match slot {
                        Slot::Card { rect } => rect,
                        Slot::Separator { rect } => rect,
                        Slot::Row { band, .. } => band,
                        _ => return,
                    };
                    assert!(
                        r.y_top >= rect.y_top - 1e-9 && r.y_top + r.h <= bottom + 1e-9,
                        "at scroll {scroll}: {r:?} is outside the panel {rect:?}"
                    );
                });
                assert!(drew > 0, "at scroll {scroll}: the panel drew nothing");
            }
        });
    }

    /// Culling is not the same as not drawing: a window with room must
    /// still emit every slot, or the fix would have hidden the panel
    /// instead of fitting it.
    #[test]
    fn a_window_with_room_culls_nothing() {
        at_scale_1(|| {
            let s = Settings::default();
            let mut m = measure();
            let rect = panel_rect(1600.0, 1200.0, &s, &mut m, 30.0);
            let mut all = 0usize;
            walk(rect, 0.0, &s, &mut measure(), |_| all += 1);
            let mut shown = 0usize;
            walk_visible(rect, 0.0, &s, &mut measure(), |_| shown += 1);
            assert_eq!(shown, all, "a panel that fits must draw all of itself");
            assert_eq!(max_scroll(rect, &s, &mut measure()), 0.0, "nowhere to scroll");
        });
    }

    /// Scrolled to the bottom, the last row is inside the panel -- the
    /// point of being able to scroll at all.
    #[test]
    fn scrolling_to_the_end_brings_the_last_row_into_view() {
        at_scale_1(|| {
            let s = Settings::default();
            let mut m = measure();
            let rect = panel_rect(1600.0, 700.0, &s, &mut m, 30.0);
            let max = max_scroll(rect, &s, &mut measure());
            assert!(max > 0.0, "a 700px window cannot show the panel whole");

            let last = |scroll: f64| {
                let mut seen = None;
                walk_visible(rect, scroll, &s, &mut measure(), |slot| {
                    if let Slot::Row { row, .. } = slot {
                        seen = Some(row);
                    }
                });
                seen
            };
            let all_last = {
                let mut seen = None;
                walk(rect, 0.0, &s, &mut measure(), |slot| {
                    if let Slot::Row { row, .. } = slot {
                        seen = Some(row);
                    }
                });
                seen.expect("the panel has rows")
            };
            assert_ne!(last(0.0), Some(all_last), "the last row was already visible");
            assert_eq!(last(max), Some(all_last), "scrolled to the end and still not there");
        });
    }

    /// The clip the painter applies has to apply to clicks too, or a
    /// row scrolled off the top answers a click aimed at the terminal
    /// behind it.
    #[test]
    fn a_click_outside_the_panel_hits_nothing() {
        at_scale_1(|| {
            let s = Settings::default();
            let mut m = measure();
            let rect = panel_rect(1600.0, 700.0, &s, &mut m, 30.0);
            let max = max_scroll(rect, &s, &mut measure());

            // Where a control sits once it has been scrolled above the
            // panel's top edge.
            let mut above: Option<(f64, f64)> = None;
            walk(rect, max, &s, &mut measure(), |slot| {
                if let Slot::Row { control, .. } = slot {
                    if control.y_top + control.h < rect.y_top && above.is_none() {
                        above = Some((control.x + control.w / 2.0, control.y_top + control.h / 2.0));
                    }
                }
            });
            let (x, y) = above.expect("scrolling to the end puts a control above the panel");
            assert_eq!(hit_test(rect, max, &s, &mut measure(), x, y), None);
        });
    }
}

/// The table, which the process monitor's two columns are built from
/// and which is the first component here that scrolls.
mod table {
    use marspot::ui::components::table::{
        ColumnWidth, RowKind, SortDir, Table, TableColumn, TableRow, TableStyle,
    };
    use marspot_term::layout::{Alignment, Rect};

    fn col(header: &str, width: ColumnWidth, align: Alignment) -> TableColumn {
        TableColumn { header: header.into(), width, align, sort: None, sortable: true }
    }

    fn rows(n: usize) -> Vec<TableRow> {
        (0..n)
            .map(|i| TableRow {
                cells: vec![format!("row {i}"), format!("{i}")],
                depth: 0,
                kind: if i % 4 == 0 { RowKind::Section } else { RowKind::Data },
            })
            .collect()
    }

    fn r(x: &Rect) -> String {
        format!("{:.1},{:.1} {:.1}x{:.1}", x.x, x.y_top, x.w, x.h)
    }

    fn table<'a>(
        cols: &'a [TableColumn],
        rows: &'a [TableRow],
        w: f64,
        h: f64,
        scroll: f64,
    ) -> Table<'a> {
        Table {
            rect: Rect { x: 100.0, y_top: 50.0, w, h },
            columns: cols,
            rows,
            style: TableStyle::default(),
            selected: Some(2),
            scroll_y: scroll,
            show_header: true,
        }
    }

    fn two_cols() -> Vec<TableColumn> {
        vec![
            col("Name", ColumnWidth::Flex(2.0), Alignment::CenterLeft),
            col("CPU%", ColumnWidth::Px(80.0), Alignment::CenterRight),
        ]
    }

    fn dump(t: &Table<'_>) -> String {
        let mut out = format!("header {}\nbody {}\n", r(&t.header_rect()), r(&t.body_rect()));
        for (i, (x, w)) in t.column_x_widths().iter().enumerate() {
            out += &format!("col[{i}] x {x:.1} w {w:.1}\n");
        }
        for i in 0..t.rows.len() {
            out += &format!("row[{i}] {}\n", r(&t.row_rect(i)));
        }
        out
    }

    /// The header is sticky and the body is what is left; rows step by
    /// `row_h` from the body's top, and the Flex column takes what the
    /// Px column does not.
    #[test]
    fn the_header_is_sticky_and_the_rows_step_below_it() {
        let cols = two_cols();
        let rs = rows(6);
        assert_eq!(
            dump(&table(&cols, &rs, 400.0, 120.0, 0.0)),
            "header 100.0,50.0 400.0x24.0\n\
             body 100.0,74.0 400.0x96.0\n\
             col[0] x 100.0 w 320.0\n\
             col[1] x 420.0 w 80.0\n\
             row[0] 100.0,74.0 400.0x22.0\n\
             row[1] 100.0,96.0 400.0x22.0\n\
             row[2] 100.0,118.0 400.0x22.0\n\
             row[3] 100.0,140.0 400.0x22.0\n\
             row[4] 100.0,162.0 400.0x22.0\n\
             row[5] 100.0,184.0 400.0x22.0\n"
        );
        let _ = SortDir::Asc;
    }

    /// Scrolling moves the rows and leaves the header where it is --
    /// which is the whole reason the header has its own rect.
    #[test]
    fn scrolling_moves_the_rows_and_not_the_header() {
        let cols = two_cols();
        let rs = rows(6);
        let t = table(&cols, &rs, 400.0, 120.0, 30.0);
        assert_eq!(r(&t.header_rect()), "100.0,50.0 400.0x24.0");
        assert_eq!(r(&t.row_rect(0)), "100.0,44.0 400.0x22.0");
        assert_eq!(r(&t.row_rect(2)), "100.0,88.0 400.0x22.0");
        // A row scrolled above the body is not clickable there.
        assert_eq!(t.hit_test_row(200.0, 50.0), None);
        assert_eq!(t.hit_test_row(200.0, 90.0), Some(2));
    }

    /// `paint` used to cull only rows *entirely* outside the body, so
    /// the one straddling the edge was drawn whole: at 400x120 the body
    /// ends at 170 and row 4 runs to 184, fourteen pixels onto whatever
    /// the table is sitting on. There is no scissor in the UI paint
    /// path, so the component has to say which rows are drawable, and
    /// `paint` has to use that and nothing else.
    #[test]
    fn only_rows_that_fit_the_body_whole_are_drawable() {
        let cols = two_cols();
        let rs = rows(6);
        let t = table(&cols, &rs, 400.0, 120.0, 0.0);
        let body = t.body_rect();
        assert_eq!(t.drawable_rows().collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        for i in t.drawable_rows() {
            let rr = t.row_rect(i);
            assert!(
                rr.y_top >= body.y_top - 1e-9 && rr.y_top + rr.h <= body.y_top + body.h + 1e-9,
                "row {i} at {rr:?} is outside the body {body:?}"
            );
        }
        // Scrolled by 8px the top row no longer fits, and the next one
        // down does not gain a place: the body is 96px, a row is 22,
        // and an 8px offset leaves 82px below the top edge -- three
        // rows, not four. A list mid-scroll shows one fewer than a
        // list at rest, which is what having no scissor costs.
        let t = table(&cols, &rs, 400.0, 120.0, 8.0);
        assert_eq!(t.drawable_rows().collect::<Vec<_>>(), vec![1, 2, 3]);
        // At exactly one row down it is back to four.
        let t = table(&cols, &rs, 400.0, 120.0, 22.0);
        assert_eq!(t.drawable_rows().collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    }

    /// Px columns adding up to more than the table used to be handed
    /// out at full width regardless, so the last ones sat past the
    /// right edge: two 300px columns in a 400px table reached 700
    /// against an edge at 500. They shrink in proportion instead, so
    /// every column keeps a share and none escapes.
    #[test]
    fn px_columns_wider_than_the_table_shrink_instead_of_overflowing() {
        let cols = vec![
            col("A", ColumnWidth::Px(300.0), Alignment::CenterLeft),
            col("B", ColumnWidth::Px(300.0), Alignment::CenterLeft),
            col("C", ColumnWidth::Flex(1.0), Alignment::CenterLeft),
        ];
        let rs = rows(1);
        let t = table(&cols, &rs, 400.0, 120.0, 0.0);
        let xw = t.column_x_widths();
        assert_eq!(
            xw.iter().map(|(x, w)| format!("{x:.1}+{w:.1}")).collect::<Vec<_>>(),
            ["100.0+200.0", "300.0+200.0", "500.0+0.0"],
        );
        let right = t.rect.x + t.rect.w;
        for (i, (x, w)) in xw.iter().enumerate() {
            assert!(x + w <= right + 1e-9, "col {i} ends at {} past {right}", x + w);
        }
    }

    /// The ordinary case is untouched: the columns still fill the table
    /// exactly, which is what the shrink must not change.
    #[test]
    fn columns_that_fit_still_fill_the_table_exactly() {
        let cols = two_cols();
        let rs = rows(1);
        let t = table(&cols, &rs, 400.0, 120.0, 0.0);
        let xw = t.column_x_widths();
        let (x, w) = xw.last().copied().unwrap();
        assert_eq!(x + w, t.rect.x + t.rect.w);
    }
}
