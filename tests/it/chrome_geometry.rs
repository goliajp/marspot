//! The rects the direct-draw chrome produces, written down.
//!
//! S3-09 moves sixteen components off direct drawing and onto the
//! declarative tree. The point of that move is CPU — H1's single-pass
//! layout and its measurement cache only pay off where the main window
//! draws at 60 fps, and today that window is drawn by code that computes
//! its own rectangles inline.
//!
//! A migration like that is supposed to move nothing on screen, and
//! `tests/it/layout_geometry.rs` already says why that cannot be checked
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
    use marspot::ui::components::panel_chrome::row_separator_rect;
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
            Slot::Group { heading, x, top, w } => {
                out += &format!("group {heading:?} at {x:.1},{top:.1} w {w:.1}\n")
            }
            Slot::Card { rect } => out += &format!("card {}\n", r(&rect)),
            Slot::Row { row, band, label_baseline, desc_baseline, control, .. } => {
                out += &format!(
                    "row {row:?} band {} label {label_baseline:.1} desc {desc_baseline:.1} ctl {}\n",
                    r(&band),
                    r(&control)
                )
            }
            Slot::Separator { card, y } => {
                out += &format!("sep {}\n", r(&row_separator_rect(card, y)))
            }
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
                "panel 360.0,71.8 880.0x1056.3\n\
                 group \"Idle reclamation\" at 396.0,151.8 w 808.0\n\
                 card 396.0,178.8 808.0x287.7\n\
                 row ReclaimEnabled band 396.0,178.8 808.0x95.9 label 222.3 desc 255.8 ctl 1112.0,196.8 68.0x40.0\n\
                 sep 420.0,274.7 760.0x1.0\n\
                 row ReclaimIdleMinutes band 396.0,274.7 808.0x95.9 label 318.2 desc 351.7 ctl 833.0,290.7 347.0x44.0\n\
                 sep 420.0,370.6 760.0x1.0\n\
                 row ReclaimPrefetch band 396.0,370.6 808.0x95.9 label 414.1 desc 447.6 ctl 1112.0,388.6 68.0x40.0\n\
                 group \"Appearance\" at 396.0,506.5 w 808.0\n\
                 card 396.0,533.5 808.0x191.8\n\
                 row DimScale band 396.0,533.5 808.0x95.9 label 577.0 desc 610.5 ctl 865.2,549.5 314.8x44.0\n\
                 sep 420.0,629.5 760.0x1.0\n\
                 row CircledWide band 396.0,629.5 808.0x95.9 label 673.0 desc 706.5 ctl 1112.0,647.5 68.0x40.0\n\
                 group \"Claude Code\" at 396.0,765.4 w 808.0\n\
                 card 396.0,792.4 808.0x95.9\n\
                 row CcStatuslineHook band 396.0,792.4 808.0x95.9 label 835.9 desc 869.4 ctl 1112.0,810.4 68.0x40.0\n\
                 group \"Scrolling\" at 396.0,928.3 w 808.0\n\
                 card 396.0,955.3 808.0x95.9\n\
                 row ScrollFactor band 396.0,955.3 808.0x95.9 label 998.8 desc 1032.3 ctl 852.0,971.3 328.0x44.0\n\
                 footer base 1092.2\n"
            );
        });
    }

    /// Narrow: the width clamps to 96% of the window and everything
    /// inside follows it -- the cards narrow, the separators narrow,
    /// and the right-aligned controls move left by the same amount.
    /// The vertical rhythm does not move, because nothing in it
    /// depends on the width.
    #[test]
    fn a_narrow_window_clamps_the_width_and_the_contents_follow() {
        at_scale_1(|| {
            assert_eq!(
                dump(800.0, 1200.0, &Settings::default()),
                "panel 16.0,71.8 768.0x1056.3\n\
                 group \"Idle reclamation\" at 52.0,151.8 w 696.0\n\
                 card 52.0,178.8 696.0x287.7\n\
                 row ReclaimEnabled band 52.0,178.8 696.0x95.9 label 222.3 desc 255.8 ctl 656.0,196.8 68.0x40.0\n\
                 sep 76.0,274.7 648.0x1.0\n\
                 row ReclaimIdleMinutes band 52.0,274.7 696.0x95.9 label 318.2 desc 351.7 ctl 377.0,290.7 347.0x44.0\n\
                 sep 76.0,370.6 648.0x1.0\n\
                 row ReclaimPrefetch band 52.0,370.6 696.0x95.9 label 414.1 desc 447.6 ctl 656.0,388.6 68.0x40.0\n\
                 group \"Appearance\" at 52.0,506.5 w 696.0\n\
                 card 52.0,533.5 696.0x191.8\n\
                 row DimScale band 52.0,533.5 696.0x95.9 label 577.0 desc 610.5 ctl 409.2,549.5 314.8x44.0\n\
                 sep 76.0,629.5 648.0x1.0\n\
                 row CircledWide band 52.0,629.5 696.0x95.9 label 673.0 desc 706.5 ctl 656.0,647.5 68.0x40.0\n\
                 group \"Claude Code\" at 52.0,765.4 w 696.0\n\
                 card 52.0,792.4 696.0x95.9\n\
                 row CcStatuslineHook band 52.0,792.4 696.0x95.9 label 835.9 desc 869.4 ctl 656.0,810.4 68.0x40.0\n\
                 group \"Scrolling\" at 52.0,928.3 w 696.0\n\
                 card 52.0,955.3 696.0x95.9\n\
                 row ScrollFactor band 52.0,955.3 696.0x95.9 label 998.8 desc 1032.3 ctl 396.0,971.3 328.0x44.0\n\
                 footer base 1092.2\n"
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
                        Slot::Separator { card, y } => row_separator_rect(card, y),
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

/// The table, which the process monitor's two cards are built from
/// and which is the first component here that scrolls.
///
/// Measured in terminal cells: a 10 x 20 cell here, so a header row is
/// 30 tall, a data row 27, a column gap 20.
mod table {
    use marspot::ui::components::table::{card_height, Align, Column, ColumnWidth, TableGeometry};
    use marspot_term::layout::Rect;

    const CELL_W: f64 = 10.0;
    const CELL_H: f64 = 20.0;

    fn col(header: &'static str, width: ColumnWidth, align: Align) -> Column {
        Column { header, width, align, sort: None }
    }

    fn r(x: &Rect) -> String {
        format!("{:.1},{:.1} {:.1}x{:.1}", x.x, x.y_top, x.w, x.h)
    }

    /// A card with room for four rows, holding `n_rows`.
    fn table(cols: &[Column], n_rows: usize, scroll: f64) -> TableGeometry {
        let card = Rect { x: 100.0, y_top: 50.0, w: 400.0, h: card_height(4, CELL_H) };
        TableGeometry::new(card, cols, n_rows, scroll, CELL_W, CELL_H)
    }

    fn two_cols() -> Vec<Column> {
        vec![
            col("Name", ColumnWidth::Flex, Align::Left),
            col("CPU%", ColumnWidth::Cells(8.0), Align::Right),
        ]
    }

    fn dump(t: &TableGeometry) -> String {
        let mut out = format!(
            "card {}\nheader {}\nseparator {:.1}\nbody {}\n",
            r(&t.card), r(&t.header), t.separator_y, r(&t.body),
        );
        for (i, (x, w)) in t.columns.iter().enumerate() {
            out += &format!("col[{i}] x {x:.1} w {w:.1}\n");
        }
        for (i, row) in &t.rows {
            out += &format!("row[{i}] {}\n", r(row));
        }
        out
    }

    fn laid_out(t: &TableGeometry) -> Vec<usize> {
        t.rows.iter().map(|(i, _)| *i).collect()
    }

    /// The header is sticky, the line sits under it, and rows step by
    /// the row height from the body's top.  Columns sit inside the
    /// card's row padding; the flex column takes what the fixed one and
    /// the gap leave.
    #[test]
    fn the_header_is_sticky_and_the_rows_step_below_it() {
        assert_eq!(
            dump(&table(&two_cols(), 6, 0.0)),
            "card 100.0,50.0 400.0x155.0\n\
             header 100.0,50.0 400.0x30.0\n\
             separator 80.0\n\
             body 100.0,89.0 400.0x108.0\n\
             col[0] x 124.0 w 252.0\n\
             col[1] x 396.0 w 80.0\n\
             row[0] 100.0,89.0 400.0x27.0\n\
             row[1] 100.0,116.0 400.0x27.0\n\
             row[2] 100.0,143.0 400.0x27.0\n\
             row[3] 100.0,170.0 400.0x27.0\n"
        );
    }

    /// Scrolling moves the rows and leaves the header where it is --
    /// which is the whole reason the header has its own rect.
    #[test]
    fn scrolling_moves_the_rows_and_not_the_header() {
        let t = table(&two_cols(), 6, 30.0);
        assert_eq!(r(&t.header), "100.0,50.0 400.0x30.0");
        assert_eq!(t.scroll, 30.0);
        assert_eq!(t.max_scroll, 54.0);
        assert_eq!(laid_out(&t), vec![2, 3, 4]);
        assert_eq!(r(&t.rows[0].1), "100.0,113.0 400.0x27.0");
        // A row scrolled above the body is not clickable there.
        assert_eq!(t.row_at(200.0, 60.0), None);
        assert_eq!(t.row_at(200.0, 95.0), None);
        assert_eq!(t.row_at(200.0, 115.0), Some(2));
        // Asking for more than the rows allow stops at the last one.
        assert_eq!(table(&two_cols(), 6, 500.0).scroll, 54.0);
    }

    /// There is no scissor in the UI paint path, so a row straddling
    /// the body's edge would be drawn whole, onto whatever the card
    /// sits on.  Only rows that fit whole are laid out, and nothing
    /// else is there to be painted or clicked.
    #[test]
    fn only_rows_that_fit_the_body_whole_are_laid_out() {
        let cols = two_cols();
        let t = table(&cols, 6, 0.0);
        assert_eq!(laid_out(&t), vec![0, 1, 2, 3]);
        for (i, rr) in &t.rows {
            assert!(
                rr.y_top >= t.body.y_top - 1e-9 && rr.y_top + rr.h <= t.body.y_top + t.body.h + 1e-9,
                "row {i} at {rr:?} is outside the body {:?}",
                t.body,
            );
        }
        // Scrolled by 8px the top row no longer fits, and the next one
        // down does not gain a place: a list mid-scroll shows one fewer
        // than a list at rest, which is what having no scissor costs.
        assert_eq!(laid_out(&table(&cols, 6, 8.0)), vec![1, 2, 3]);
        // At exactly one row down it is back to four.
        assert_eq!(laid_out(&table(&cols, 6, 27.0)), vec![1, 2, 3, 4]);
    }

    /// Fixed columns adding up to more than the card shrink in
    /// proportion, gaps included, so every column keeps a share and
    /// none escapes the card's padding.
    #[test]
    fn columns_wider_than_the_card_shrink_instead_of_overflowing() {
        let cols = vec![
            col("A", ColumnWidth::Cells(30.0), Align::Left),
            col("B", ColumnWidth::Cells(30.0), Align::Left),
            col("C", ColumnWidth::Flex, Align::Left),
        ];
        let t = table(&cols, 1, 0.0);
        assert_eq!(
            t.columns.iter().map(|(x, w)| format!("{x:.1}+{w:.1}")).collect::<Vec<_>>(),
            ["124.0+165.0", "300.0+165.0", "476.0+0.0"],
        );
    }

    /// The ordinary case: the columns end exactly at the card's row
    /// padding, which is what the shrink must not change.
    #[test]
    fn columns_that_fit_end_at_the_row_padding() {
        let t = table(&two_cols(), 1, 0.0);
        let (x, w) = t.columns.last().copied().unwrap();
        assert_eq!(x + w, 476.0);
    }
}
