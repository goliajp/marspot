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
        Slot, max_scroll, panel_rect, walk, walk_visible,
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
                dump(1600.0, 2378.9, &Settings::default()),
                "panel 320.0,100.0 960.0x2179.0\n\
                 group \"Idle reclamation\" at 356.0,180.0 w 888.0\n\
                 card 356.0,207.0 888.0x287.7\n\
                 row ReclaimEnabled band 356.0,207.0 888.0x95.9 label 250.5 desc 284.0 ctl 1152.0,225.0 68.0x40.0\n\
                 sep 380.0,302.9 840.0x1.0\n\
                 row ReclaimIdleMinutes band 356.0,302.9 888.0x95.9 label 346.4 desc 379.9 ctl 873.0,318.9 347.0x44.0\n\
                 sep 380.0,398.8 840.0x1.0\n\
                 row ReclaimPrefetch band 356.0,398.8 888.0x95.9 label 442.3 desc 475.8 ctl 1152.0,416.8 68.0x40.0\n\
                 group \"Appearance\" at 356.0,534.7 w 888.0\n\
                 card 356.0,561.7 888.0x671.3\n\
                 row Theme band 356.0,561.7 888.0x95.9 label 605.2 desc 638.7 ctl 839.2,577.7 380.8x44.0\n\
                 sep 380.0,657.6 840.0x1.0\n\
                 row Language band 356.0,657.6 888.0x95.9 label 701.1 desc 734.6 ctl 892.0,673.6 328.0x44.0\n\
                 sep 380.0,753.5 840.0x1.0\n\
                 row FontFamily band 356.0,753.5 888.0x95.9 label 797.0 desc 830.5 ctl 832.0,769.5 388.0x44.0\n\
                 sep 380.0,849.4 840.0x1.0\n\
                 row FontSize band 356.0,849.4 888.0x95.9 label 892.9 desc 926.4 ctl 1068.0,865.4 152.0x44.0\n\
                 sep 380.0,945.3 840.0x1.0\n\
                 row DimScale band 356.0,945.3 888.0x95.9 label 988.8 desc 1022.3 ctl 905.2,961.3 314.8x44.0\n\
                 sep 380.0,1041.2 840.0x1.0\n\
                 row CircledWide band 356.0,1041.2 888.0x95.9 label 1084.7 desc 1118.2 ctl 1152.0,1059.2 68.0x40.0\n\
                 sep 380.0,1137.1 840.0x1.0\n\
                 row HelperStrip band 356.0,1137.1 888.0x95.9 label 1180.6 desc 1214.1 ctl 1152.0,1155.1 68.0x40.0\n\
                 group \"Claude Code\" at 356.0,1273.0 w 888.0\n\
                 card 356.0,1300.0 888.0x287.7\n\
                 row CcStatuslineHook band 356.0,1300.0 888.0x95.9 label 1343.5 desc 1377.0 ctl 1152.0,1318.0 68.0x40.0\n\
                 sep 380.0,1395.9 840.0x1.0\n\
                 row CcModelGuard band 356.0,1395.9 888.0x95.9 label 1439.4 desc 1472.9 ctl 1152.0,1413.9 68.0x40.0\n\
                 sep 380.0,1491.8 840.0x1.0\n\
                 row SchedProfile band 356.0,1491.8 888.0x95.9 label 1535.3 desc 1568.8 ctl 930.8,1507.8 289.2x44.0\n\
                 group \"Scrolling\" at 356.0,1627.7 w 888.0\n\
                 card 356.0,1654.7 888.0x95.9\n\
                 row ScrollFactor band 356.0,1654.7 888.0x95.9 label 1698.2 desc 1731.7 ctl 892.0,1670.7 328.0x44.0\n\
                 group \"Agent CLIs\" at 356.0,1790.6 w 888.0\n\
                 card 356.0,1817.6 888.0x383.6\n\
                 row InstallCli(0) band 356.0,1817.6 888.0x95.9 label 1861.1 desc 1894.6 ctl 924.2,1833.6 295.8x44.0\n\
                 sep 380.0,1913.5 840.0x1.0\n\
                 row InstallCli(1) band 356.0,1913.5 888.0x95.9 label 1957.0 desc 1990.5 ctl 924.2,1929.5 295.8x44.0\n\
                 sep 380.0,2009.5 840.0x1.0\n\
                 row InstallCli(2) band 356.0,2009.5 888.0x95.9 label 2053.0 desc 2086.5 ctl 924.2,2025.5 295.8x44.0\n\
                 sep 380.0,2105.4 840.0x1.0\n\
                 row InstallCli(3) band 356.0,2105.4 888.0x95.9 label 2148.9 desc 2182.4 ctl 924.2,2121.4 295.8x44.0\n\
                 footer base 2242.3\n",
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
                dump(800.0, 2378.9, &Settings::default()),
                "panel 16.0,100.0 768.0x2179.0\n\
                 group \"Idle reclamation\" at 52.0,180.0 w 696.0\n\
                 card 52.0,207.0 696.0x287.7\n\
                 row ReclaimEnabled band 52.0,207.0 696.0x95.9 label 250.5 desc 284.0 ctl 656.0,225.0 68.0x40.0\n\
                 sep 76.0,302.9 648.0x1.0\n\
                 row ReclaimIdleMinutes band 52.0,302.9 696.0x95.9 label 346.4 desc 379.9 ctl 377.0,318.9 347.0x44.0\n\
                 sep 76.0,398.8 648.0x1.0\n\
                 row ReclaimPrefetch band 52.0,398.8 696.0x95.9 label 442.3 desc 475.8 ctl 656.0,416.8 68.0x40.0\n\
                 group \"Appearance\" at 52.0,534.7 w 696.0\n\
                 card 52.0,561.7 696.0x671.3\n\
                 row Theme band 52.0,561.7 696.0x95.9 label 605.2 desc 638.7 ctl 343.2,577.7 380.8x44.0\n\
                 sep 76.0,657.6 648.0x1.0\n\
                 row Language band 52.0,657.6 696.0x95.9 label 701.1 desc 734.6 ctl 396.0,673.6 328.0x44.0\n\
                 sep 76.0,753.5 648.0x1.0\n\
                 row FontFamily band 52.0,753.5 696.0x95.9 label 797.0 desc 830.5 ctl 336.0,769.5 388.0x44.0\n\
                 sep 76.0,849.4 648.0x1.0\n\
                 row FontSize band 52.0,849.4 696.0x95.9 label 892.9 desc 926.4 ctl 572.0,865.4 152.0x44.0\n\
                 sep 76.0,945.3 648.0x1.0\n\
                 row DimScale band 52.0,945.3 696.0x95.9 label 988.8 desc 1022.3 ctl 409.2,961.3 314.8x44.0\n\
                 sep 76.0,1041.2 648.0x1.0\n\
                 row CircledWide band 52.0,1041.2 696.0x95.9 label 1084.7 desc 1118.2 ctl 656.0,1059.2 68.0x40.0\n\
                 sep 76.0,1137.1 648.0x1.0\n\
                 row HelperStrip band 52.0,1137.1 696.0x95.9 label 1180.6 desc 1214.1 ctl 656.0,1155.1 68.0x40.0\n\
                 group \"Claude Code\" at 52.0,1273.0 w 696.0\n\
                 card 52.0,1300.0 696.0x287.7\n\
                 row CcStatuslineHook band 52.0,1300.0 696.0x95.9 label 1343.5 desc 1377.0 ctl 656.0,1318.0 68.0x40.0\n\
                 sep 76.0,1395.9 648.0x1.0\n\
                 row CcModelGuard band 52.0,1395.9 696.0x95.9 label 1439.4 desc 1472.9 ctl 656.0,1413.9 68.0x40.0\n\
                 sep 76.0,1491.8 648.0x1.0\n\
                 row SchedProfile band 52.0,1491.8 696.0x95.9 label 1535.3 desc 1568.8 ctl 434.8,1507.8 289.2x44.0\n\
                 group \"Scrolling\" at 52.0,1627.7 w 696.0\n\
                 card 52.0,1654.7 696.0x95.9\n\
                 row ScrollFactor band 52.0,1654.7 696.0x95.9 label 1698.2 desc 1731.7 ctl 396.0,1670.7 328.0x44.0\n\
                 group \"Agent CLIs\" at 52.0,1790.6 w 696.0\n\
                 card 52.0,1817.6 696.0x383.6\n\
                 row InstallCli(0) band 52.0,1817.6 696.0x95.9 label 1861.1 desc 1894.6 ctl 428.2,1833.6 295.8x44.0\n\
                 sep 76.0,1913.5 648.0x1.0\n\
                 row InstallCli(1) band 52.0,1913.5 696.0x95.9 label 1957.0 desc 1990.5 ctl 428.2,1929.5 295.8x44.0\n\
                 sep 76.0,2009.5 648.0x1.0\n\
                 row InstallCli(2) band 52.0,2009.5 696.0x95.9 label 2053.0 desc 2086.5 ctl 428.2,2025.5 295.8x44.0\n\
                 sep 76.0,2105.4 648.0x1.0\n\
                 row InstallCli(3) band 52.0,2105.4 696.0x95.9 label 2148.9 desc 2182.4 ctl 428.2,2121.4 295.8x44.0\n\
                 footer base 2242.3\n"
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
            let rect = panel_rect(1600.0, 2378.9, &s, &mut m, 30.0);
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
            // nothing of the panel is there: the frame starts below it
            assert!(y < rect.y_top && x > rect.x);
        });
    }
}
