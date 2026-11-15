//! The chrome's hit test and hover, through the trees the toolbar, the
//! sidebar and the title strips are drawn from.

use super::sidebar_view::{SidebarFrame, SidebarSurface};
use super::toolbar_view_tests::{Frame, Stage};
use crate::font_cache::FontCache;
use crate::frame_alloc_tests::counted;
use crate::frame_build::chrome::{ChromeShape, ChromeSurfaces};
use crate::layout::Layout;
use crate::render::SidebarEntry;
use crate::ui::components::chrome_target::ChromeTarget;
use crate::ui::components::density_kit::at_scale;
use crate::ui::components::title_strip_view::BadgesOnly;
use marspot_term::session::SessionState;

const BADGES: [&str; 4] = ["P1 3f9c2a1e", "", "P2 77d0e4b2", ""];

fn layout(font: &FontCache, n: usize) -> Layout {
    Layout::build(
        1400.0,
        900.0,
        crate::ui::SIDEBAR_W_LOGICAL,
        crate::HEADER_PT,
        crate::ui::CELL_TITLE_PT,
        2,
        2,
        font.cell_w,
        font.cell_h,
    )
    .with_chrome(1.0, n, None)
}

fn mid(r: crate::layout::Rect) -> (f64, f64) {
    (r.x + r.w * 0.5, r.y_top + r.h * 0.5)
}

fn row_at(l: &Layout, i: usize, x: f64) -> (f64, f64) {
    let row_h = marspot_term::layout::sidebar_row_h();
    (x, l.top_inset + l.sidebar_top_pad_phys + (i as f64 + 0.5) * row_h)
}

/// Where each control is, it is what a point there is on; nothing is
/// in a pane, below the last row or in the window's corner.
#[test]
fn a_point_is_on_the_control_drawn_there_and_on_nothing_elsewhere() {
    at_scale(1.0, || {
        let mut font = FontCache::build().expect("font");
        let l = layout(&font, 3);
        let strips = BadgesOnly(3, |i| BADGES[i]);
        let shape = ChromeShape { layout: &l, rows: 3, strips: &strips };
        let mut c = ChromeSurfaces::default();
        let cell_w = font.cell_w;
        let mut at = |p| c.at(&mut font, &shape, p);
        assert_eq!(at(mid(l.sidebar_button_rect)), Some(ChromeTarget::Toolbar(0)));
        assert_eq!(at(mid(l.settings_button_rect)), Some(ChromeTarget::Toolbar(4)));
        assert_eq!(at(mid(l.add_session_button_rect)), Some(ChromeTarget::Add));
        assert_eq!(at(mid(l.close_session_rects[1])), Some(ChromeTarget::Close(1)));
        assert_eq!(at(row_at(&l, 1, 30.0)), Some(ChromeTarget::Row(1)));
        let cell = l.cells[2];
        assert_eq!(at((cell.x + 5.0, cell.y_top + 2.0)), Some(ChromeTarget::Title(2)));
        // the badge's prefix, "P2", flush right
        let badge_x = cell.x + cell.w - l.padding - BADGES[2].chars().count() as f64 * cell_w;
        assert_eq!(at((badge_x + 0.5 * cell_w, cell.y_top + 2.0)), Some(ChromeTarget::Badge(2)));
        // past the prefix, on the rest of the badge, is the strip
        assert_eq!(at((badge_x + 3.5 * cell_w, cell.y_top + 2.0)), Some(ChromeTarget::Title(2)));
        // no badge on the second pane, and the fourth cell has no pane
        let cell = l.cells[3];
        assert_eq!(at((cell.x + cell.w - 4.0, cell.y_top + 2.0)), None);
        assert_eq!(at((700.0, 400.0)), None);
        assert_eq!(at(row_at(&l, 3, 30.0)), None);
        assert_eq!(at((0.0, 0.0)), None);
    });
}

/// Input that arrives before the frame that shows a new layout is
/// tested against that layout, not the one last drawn; and when
/// nothing moved, asking describes and lays out nothing.
#[test]
fn a_press_before_the_next_frame_lands_where_that_frame_draws() {
    at_scale(1.0, || {
        let mut font = FontCache::build().expect("font");
        let mut c = ChromeSurfaces::default();
        let (three, four) = (layout(&font, 3), layout(&font, 4));
        let strips = BadgesOnly(3, |i| BADGES[i]);
        let hidden = Layout::build(1400.0, 900.0, 0.0, crate::HEADER_PT, crate::ui::CELL_TITLE_PT, 2, 2, font.cell_w, font.cell_h)
            .with_chrome(1.0, 4, None);
        let s3 = ChromeShape { layout: &three, rows: 3, strips: &strips };
        let s4 = ChromeShape { layout: &four, rows: 4, strips: &strips };
        let s_hidden = ChromeShape { layout: &hidden, rows: 4, strips: &strips };
        assert_eq!(c.at(&mut font, &s3, row_at(&three, 3, 30.0)), None);
        let sidebar = c.sidebar.kept.surface.counters();
        assert_eq!(c.at(&mut font, &s3, row_at(&three, 2, 30.0)), Some(ChromeTarget::Row(2)));
        assert_eq!(c.sidebar.kept.surface.counters(), sidebar, "nothing moved, and the sidebar was laid out again");
        // a fourth pane, and no frame since: the pointer lights its row
        // and a press lands on it; first with the layout made for it and
        // the sidebar still listing three, then listing four
        let listing_three = ChromeShape { layout: &four, rows: 3, strips: &strips };
        assert!(!c.pointer_move(&mut font, &listing_three, row_at(&four, 3, 30.0)), "a fourth row lit with three listed");
        assert!(c.pointer_move(&mut font, &s4, row_at(&four, 3, 30.0)), "the new row did not light");
        assert_eq!(c.sidebar.kept.surface.interaction.hovered.map(|t| t.id), Some(golia_ui_core::WidgetId(4)));
        assert_eq!(c.at(&mut font, &s4, row_at(&four, 3, 30.0)), Some(ChromeTarget::Row(3)));
        assert_eq!(c.at(&mut font, &s4, mid(four.close_session_rects[3])), Some(ChromeTarget::Close(3)));
        // the sidebar hidden
        assert_eq!(c.at(&mut font, &s_hidden, row_at(&four, 1, 30.0)), None);
        // a badge that arrives moves the prefix with it
        let cell = four.cells[1];
        let p = (cell.x + cell.w - four.padding - 1.0, cell.y_top + 2.0);
        assert_eq!(c.at(&mut font, &s4, p), Some(ChromeTarget::Title(1)));
        let badged = BadgesOnly(3, |i| if i == 1 { "Z" } else { BADGES[i] });
        let s = ChromeShape { layout: &four, rows: 4, strips: &badged };
        assert_eq!(c.at(&mut font, &s, p), Some(ChromeTarget::Badge(1)));
    });
}

/// A button acts when it is let go on, having been pressed on; let go
/// elsewhere, or in another window, it does nothing, and a release
/// with no press before it is no click.
#[test]
fn a_button_acts_on_the_release_that_follows_its_press() {
    at_scale(1.0, || {
        let mut font = FontCache::build().expect("font");
        let l = layout(&font, 3);
        let strips = BadgesOnly(3, |i| BADGES[i]);
        let shape = ChromeShape { layout: &l, rows: 3, strips: &strips };
        let mut c = ChromeSurfaces::default();
        let close = mid(l.close_session_rects[2]);
        let add = mid(l.add_session_button_rect);
        let button = mid(l.toolbar_buttons()[2]);
        assert_eq!(c.press(&mut font, &shape, close), Some(ChromeTarget::Close(2)));
        assert_eq!(c.release(&mut font, &shape, Some((close.0 - 1.0, close.1))), Some(ChromeTarget::Close(2)));
        assert_eq!(c.press(&mut font, &shape, close), Some(ChromeTarget::Close(2)));
        assert_eq!(c.release(&mut font, &shape, Some(add)), None, "slid onto another button");
        assert_eq!(c.press(&mut font, &shape, button), Some(ChromeTarget::Toolbar(2)));
        assert_eq!(c.release(&mut font, &shape, Some((700.0, 400.0))), None, "slid off the chrome");
        assert_eq!(c.press(&mut font, &shape, add), Some(ChromeTarget::Add));
        assert_eq!(c.release(&mut font, &shape, None), None, "let go in another window");
        assert_eq!(c.release(&mut font, &shape, Some(add)), None, "a release with no press");
        assert_eq!(c.press(&mut font, &shape, button), Some(ChromeTarget::Toolbar(2)));
        assert_eq!(c.release(&mut font, &shape, Some(button)), Some(ChromeTarget::Toolbar(2)));
    });
}

/// The pointer moving over the sidebar lights a row and its close
/// button together, whichever of the two it is on, and only paints:
/// nothing is described or laid out again, nothing is allocated.
#[test]
fn a_row_and_its_close_button_light_together_with_a_paint_and_nothing_else() {
    at_scale(1.0, || {
        let Some(mut stage) = Stage::new() else { return };
        let window = (1400.0, 900.0);
        let l = layout(&stage.font, 3);
        let names = [
            SidebarEntry { label: "smix", state: SessionState::Active },
            SidebarEntry { label: "devops", state: SessionState::Idle },
            SidebarEntry { label: "alt", state: SessionState::Exited },
        ];
        let rows: &[SidebarEntry] = &names;
        let cell_w = stage.font.cell_w as f32;
        let f = SidebarFrame { layout: &l, rows: &rows, focused_idx: 0, quiet: false, cell_w };
        let mut s = SidebarSurface::default();
        let picture = |stage: &mut Stage, s: &mut SidebarSurface, p: (f64, f64)| -> Frame {
            s.kept.surface.pointer_move(p);
            let mut out: Frame = Default::default();
            stage.draw(window, &mut out, &mut |pa| s.frame(pa, &f));
            out
        };
        let off = (700.0, 400.0);
        let rest = picture(&mut stage, &mut s, off);
        let on_row = picture(&mut stage, &mut s, row_at(&l, 1, 30.0));
        let on_close = picture(&mut stage, &mut s, mid(l.close_session_rects[1]));
        // a row's lit fill: as tall as the row, most of the sidebar wide,
        // at the second row's height
        let (_, y1) = row_at(&l, 1, 0.0);
        let row_fills = |f: &Frame| {
            f.2.iter()
                .filter(|r| r.size[0] as f64 > l.sidebar_w * 0.5 && (r.origin[1] as f64) < y1 && ((r.origin[1] + r.size[1]) as f64) > y1)
                .count()
        };
        assert_eq!(row_fills(&rest), 0);
        assert_eq!(row_fills(&on_row), 1, "a row lit under the pointer");
        assert_eq!(row_fills(&on_close), 1, "the row went dark when the pointer moved onto its close button");
        // and the close button shows on the lit row, hot under the
        // pointer and not when it is only lit with its row
        assert!(on_row.2.len() > rest.2.len(), "no close button on the lit row");
        assert_ne!(on_close, on_row, "the close button under the pointer looks as it does beside it");

        picture(&mut stage, &mut s, off);
        let before = s.kept.surface.counters();
        // input asks the sidebar to be ready for it: nothing moved since
        // the frame, so the frame's tree stays as it was described
        s.ready(&mut stage.font, &l, names.len());
        assert_eq!(s.kept.surface.counters(), before, "a frame's sidebar was laid out again for input");
        let (n, _) = counted(|| {
            for p in [row_at(&l, 2, 30.0), mid(l.close_session_rects[2]), row_at(&l, 0, 30.0), off] {
                s.kept.surface.pointer_move(p);
            }
        });
        assert_eq!(n, 0, "moving the pointer allocated {n} times");
        let back = picture(&mut stage, &mut s, off);
        let after = s.kept.surface.counters();
        assert_eq!((after.build, after.layout), (before.build, before.layout), "a hover described or laid out");
        assert_eq!(back, rest);
    });
}

/// Under a panel or a menu nothing on the sidebar lights, though what
/// the pointer is on is still followed, so it lights when that closes.
#[test]
fn under_a_panel_the_sidebar_follows_the_pointer_and_lights_nothing() {
    at_scale(1.0, || {
        let Some(mut stage) = Stage::new() else { return };
        let window = (1400.0, 900.0);
        let l = layout(&stage.font, 3);
        let entry = |label| SidebarEntry { label, state: SessionState::Active };
        let names = [entry("smix"), entry("devops"), entry("alt")];
        let rows: &[SidebarEntry] = &names;
        let cell_w = stage.font.cell_w as f32;
        let open = SidebarFrame { layout: &l, rows: &rows, focused_idx: 0, quiet: true, cell_w };
        let closed = SidebarFrame { quiet: false, ..open };
        let mut s = SidebarSurface::default();
        let mut draw = |s: &mut SidebarSurface, f: &SidebarFrame<'_>| -> Frame {
            let mut out: Frame = Default::default();
            stage.draw(window, &mut out, &mut |pa| s.frame(pa, f));
            out
        };
        let dark = draw(&mut s, &open);
        assert!(s.kept.surface.pointer_move(mid(l.close_session_rects[2])));
        assert_eq!(draw(&mut s, &open), dark, "a control lit under a panel");
        let lit = draw(&mut s, &closed);
        s.kept.surface.pointer_leave();
        assert_ne!(lit, draw(&mut s, &closed), "the control under the pointer did not light when the panel closed");
    });
}

/// A badge wider than its pane is clickable only inside its own pane:
/// on the strip of the pane to its left, where nothing of it is drawn,
/// a press is that pane's, not the plugin's of the pane on the right.
#[test]
fn a_badge_wider_than_its_pane_stops_at_its_panes_edge() {
    at_scale(1.0, || {
        let mut font = FontCache::build().expect("font");
        let l = layout(&font, 2);
        // no space, so the clickable prefix is the whole badge, and it
        // is wider than any pane in the window
        let wide: String = "P2-".chars().chain(std::iter::repeat_n('x', 400)).collect();
        let badges = |i: usize| if i == 1 { wide.as_str() } else { "" };
        let strips = BadgesOnly(2, badges);
        let shape = ChromeShape { layout: &l, rows: 2, strips: &strips };
        let mut c = ChromeSurfaces::default();
        let (left, right) = (l.cells[0], l.cells[1]);
        let reach = right.x + right.w - l.padding - wide.chars().count() as f64 * font.cell_w;
        assert!(reach < left.x, "the badge has to reach past the pane on the left for this to show anything");
        let y = left.y_top + 2.0;
        assert_eq!(c.at(&mut font, &shape, (left.x + left.w - 5.0, y)), Some(ChromeTarget::Title(0)));
        assert_eq!(c.at(&mut font, &shape, (left.x + 5.0, y)), Some(ChromeTarget::Title(0)));
        assert_eq!(c.at(&mut font, &shape, (right.x + 5.0, y)), Some(ChromeTarget::Badge(1)));
    });
}
