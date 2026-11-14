//! Two paint paths that claim to draw the same thing, held to the same
//! bytes.
//!
//! The panel snapshots compare with a committed PNG by structural
//! similarity, and that threshold has room for a rectangle one pixel
//! off.  Moving a panel from one paint path to another is supposed to
//! move nothing, so the two paths are rendered here, offscreen, with a
//! fresh renderer each, and every byte of the two RGBA buffers has to
//! agree.
//!
//! Each render builds its own renderer so the glyph atlas starts empty
//! both times: the same glyphs then land in the same slots, and a
//! difference in the picture is a difference in what was painted.

use crate::frame_build::frame::TEST_OVERLAY;
use crate::grid::Grid;
use crate::layout::Layout;
use crate::render::SessionView;
use crate::render_metal::{MetalRenderer, WindowRender, make_readback_texture};
use crate::ui::core::ViewPainter;
use marspot_term::layout::Rect;

pub(crate) type Paint = fn(&mut ViewPainter<'_>);

pub(crate) const W: u32 = 640;
pub(crate) const H: u32 = 400;

/// RGBA of one `W × H` frame with `paint` drawn over an empty
/// terminal.  `None` where there is no Metal device to draw with.
pub(crate) fn render(paint: Paint) -> Option<Vec<u8>> {
    render_sized(paint, W, H)
}

/// [`render`] for a frame of `W × H` pixels, built at the display
/// scale in force.
pub(crate) fn render_sized(paint: Paint, w: u32, h: u32) -> Option<Vec<u8>> {
    render_with(Some(paint), w, h, |_, _| {})
}

/// [`render_sized`] with the renderer handed to `setup` before the
/// frame is drawn, for what a frame shows that no overlay paints.
pub(crate) fn render_with(
    paint: Option<Paint>,
    w: u32,
    h: u32,
    setup: impl FnOnce(&mut MetalRenderer, &Layout),
) -> Option<Vec<u8>> {
    let mut renderer = match MetalRenderer::new_headless() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            return None;
        }
    };
    let target = make_readback_texture(renderer.device(), w, h).expect("a readback texture");
    let (cell_w, cell_h) = renderer.cell_dims();
    let layout = Layout::build(w as f64, h as f64, 0.0, 0.0, 0.0, 1, 1, cell_w, cell_h);
    let grid = Grid::new(((w as f64 / cell_w) as u16).max(8), ((h as f64 / cell_h) as u16).max(4));
    let view = SessionView {
        grid: &grid,
        view_offset: 0,
        cursor_visible: false,
        focused: true,
        title: "",
        selection: None,
        ime_preedit: "",
        dormant: false,
        recede: 0,
        scrim: 0.0,
        right_badge: "",
        agent_tui: false,
        cwd: "",
        top_fixed_h_cells: 0,
        bot_fixed_h_cells: 0,
        highlight_spans: &[],
        search_overlay: None,
        seq: 1,
    };
    *TEST_OVERLAY.lock().unwrap_or_else(|e| e.into_inner()) = paint;
    setup(&mut renderer, &layout);
    let mut wr = WindowRender::new();
    let views = std::slice::from_ref(&view);
    // twice: glyphs rasterised during the first frame land in the
    // atlas texture the second one samples
    renderer.render_layout_to_texture(&mut wr, &target, &layout, views, &[], 0);
    renderer.render_layout_to_texture(&mut wr, &target, &layout, views, &[], 0);
    *TEST_OVERLAY.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let mut rgba = renderer.read_target(&target).expect("readback");
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Some(rgba)
}

/// `(x, y)` of the first pixel the two buffers disagree on, and how
/// many disagree in all.
pub(crate) fn first_difference(a: &[u8], b: &[u8]) -> Option<((u32, u32), usize)> {
    first_difference_in(a, b, W)
}

/// [`first_difference`] for frames `w` pixels wide.
pub(crate) fn first_difference_in(a: &[u8], b: &[u8], w: u32) -> Option<((u32, u32), usize)> {
    assert_eq!(a.len(), b.len(), "the two frames are different sizes");
    let mut first = None;
    let mut count = 0usize;
    for (i, (pa, pb)) in a.chunks_exact(4).zip(b.chunks_exact(4)).enumerate() {
        if pa != pb {
            count += 1;
            first.get_or_insert((i as u32 % w, i as u32 / w));
        }
    }
    first.map(|at| (at, count))
}

/// Render both and require the same bytes.  Returns false where the
/// comparison could not run at all, so a caller can tell "equal" from
/// "skipped".
pub(crate) fn assert_same_pixels(name: &str, a: Paint, b: Paint) -> bool {
    assert_same_pixels_sized(name, a, b, W, H)
}

/// [`assert_same_pixels`] on frames of `w × h` pixels.
pub(crate) fn assert_same_pixels_sized(name: &str, a: Paint, b: Paint, w: u32, h: u32) -> bool {
    let (Some(pa), Some(pb)) = (render_sized(a, w, h), render_sized(b, w, h)) else { return false };
    let flat = [pa[0], pa[1], pa[2], pa[3]];
    assert!(
        pa.chunks_exact(4).any(|px| px != flat),
        "{name}: the frame is one flat colour, so equal frames would prove nothing",
    );
    if let Some(((x, y), n)) = first_difference_in(&pa, &pb, w) {
        panic!("{name}: {n} pixels differ, the first at ({x}, {y})");
    }
    true
}

fn sample(p: &mut ViewPainter<'_>, radius: f32) {
    p.fill_rect(Rect { x: 40.0, y_top: 40.0, w: 560.0, h: 320.0 }, [0.10, 0.11, 0.13, 1.0]);
    p.fill_rounded_rect(
        Rect { x: 80.0, y_top: 80.0, w: 240.0, h: 96.0 },
        [0.20, 0.45, 0.90, 1.0],
        radius,
        ([1.0, 1.0, 1.0, 0.4], 2.0),
    );
    p.ui_text_at(100.0, 140.0, "Same bytes", 13.0, 500, [1.0, 1.0, 1.0, 1.0]);
    p.ui_text_at(100.0, 240.0, "or it moved", 11.0, 400, [0.8, 0.8, 0.8, 1.0]);
}

fn sample_r8(p: &mut ViewPainter<'_>) {
    sample(p, 8.0);
}

/// The same picture through a second function, standing in for a
/// second paint path.
fn sample_r8_again(p: &mut ViewPainter<'_>) {
    sample(p, 8.0);
}

fn sample_r9(p: &mut ViewPainter<'_>) {
    sample(p, 9.0);
}

#[test]
fn the_same_paint_twice_is_the_same_bytes() {
    assert_same_pixels("sample", sample_r8, sample_r8_again);
}

/// One corner radius, one pixel larger: the comparison has to go red,
/// or equal frames above mean nothing.
#[test]
fn one_changed_radius_is_noticed() {
    let (Some(a), Some(b)) = (render(sample_r8), render(sample_r9)) else { return };
    let ((x, y), n) = first_difference(&a, &b).expect("a radius of 8 and of 9 drew the same bytes");
    assert!(
        (80..320).contains(&x) && (80..176).contains(&y),
        "the difference is at ({x}, {y}), outside the rectangle whose radius changed",
    );
    assert!(n < 400, "{n} pixels moved for one radius step; something else differs too");

    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(|| assert_same_pixels("control", sample_r8, sample_r9));
    std::panic::set_hook(prev);
    assert!(r.is_err(), "the assertion passed two different pictures");
}

#[test]
fn the_difference_finder_sees_one_byte() {
    let a = vec![0u8; (W * H * 4) as usize];
    let mut b = a.clone();
    assert_eq!(first_difference(&a, &b), None);
    b[((3 * W + 5) * 4 + 1) as usize] = 1;
    assert_eq!(first_difference(&a, &b), Some(((5, 3), 1)));
}
