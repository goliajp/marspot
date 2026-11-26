//! Reused instance buffers: the pixels match a fresh fill, and each
//! window fills its own.

use super::*;

/// End-to-end pixel guard for the reused instance buffers.
///
/// Nothing covered this path before: the suite's only
/// `render_layout_to_texture` remark says a real readback "needs a
/// `StorageModeShared` target texture that the renderer doesn't
/// currently expose", and left it there — so the frame L2 actually
/// paints had no pixel assertion at all.  That was tolerable while
/// every pass allocated a fresh buffer; it is not tolerable now
/// that they are refilled in place, because the way that fails is
/// silently, in pixels, on the frame after a bigger one.
///
/// The contract stated as a test: **a frame drawn into reused
/// buffers is byte-identical to the same frame drawn into fresh
/// ones**, including when a larger frame ran in between and left
/// its tail in the buffer.
#[test]
fn reused_instance_buffers_paint_the_same_pixels() {
    use crate::grid::{Cell, Grid};
    use crate::layout::Layout;

    let mut renderer = match MetalRenderer::new_headless() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            return;
        }
    };
    // Shared storage so the CPU can read it back without a blit —
    // the thing the old comment said was missing.
    let device = renderer.device().to_owned();
    let (w, h) = (320u32, 160u32);
    let desc = unsafe {
        objc2_metal::MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            TARGET_FORMAT, w as usize, h as usize, false,
        )
    };
    desc.setUsage(
        objc2_metal::MTLTextureUsage::RenderTarget | objc2_metal::MTLTextureUsage::ShaderRead,
    );
    desc.setStorageMode(objc2_metal::MTLStorageMode::Shared);
    let target = match device.newTextureWithDescriptor(&desc) {
        Some(t) => t,
        None => panic!("a Metal device that cannot make a 320x160 shared texture"),
    };

    let (cell_w, cell_h) = renderer.cell_dims();
    let layout = Layout::build(w as f64, h as f64, 0.0, 0.0, 0.0, 1, 1, cell_w, cell_h);
    let mut wr = WindowRender::new();

    let scene = |text: &str, cols: u16, rows: u16| -> Grid {
        let mut g = Grid::new(cols, rows);
        for (r, line) in text.lines().enumerate() {
            for (c, ch) in line.chars().enumerate() {
                g.set_cell(c as u16, r as u16, Cell { ch, attrs: Default::default() });
            }
        }
        g
    };
    // `seq` is what the per-pane instance cache fingerprints —
    // the grid's contents are never hashed, because the session
    // bumps `seq` whenever they change.  A first draft of this
    // test held `seq` at 0 while swapping the grid underneath,
    // and every frame came back identical: the cache was right
    // and the test was lying to it.
    fn view_of(g: &Grid, seq: u64) -> SessionView<'_> {
        SessionView {
            grid: g,
            view_offset: 0,
            cursor_visible: false,
            focused: true,
            title: "",
            selection: None,
            ime_preedit: "",
            dormant: false,
            recede: 0,
            scrim: 0.0,
            right_badge: "", agent_tui: false, cwd: "",
            top_fixed_h_cells: 0,
            bot_fixed_h_cells: 0,
            highlight_spans: &[],
            search_overlay: None,
            history: Default::default(), session: 0, strip: None, notice: None,
            seq,
        }
    }

    let small = scene("hi", 20, 6);
    let big = scene(
        "MMMMMMMMMMMMMMMMMMMM\nMMMMMMMMMMMMMMMMMMMM\nMMMMMMMMMMMMMMMMMMMM\nMMMMMMMMMMMMMMMMMMMM\nMMMMMMMMMMMMMMMMMMMM\nMMMMMMMMMMMMMMMMMMMM",
        20, 6,
    );

    // Frame 1: cold pool — every buffer is freshly allocated.
    let v = view_of(&small, 1);
    renderer.render_layout_to_texture(&mut wr, &target, &layout, std::slice::from_ref(&v), &[], 0);
    let fresh = texture_bytes_bgra(&target);
    {
        // Diagnostic: the BG pass always clears to SIDEBAR_BG, a
        // dark grey — pure black everywhere means the render never
        // reached this texture, which is a broken test rig, not a
        // broken renderer.
        let mut distinct = std::collections::HashSet::new();
        for px in fresh.as_chunks::<4>().0 {
            distinct.insert([px[0], px[1], px[2], px[3]]);
            if distinct.len() > 8 { break }
        }
        let sample: Vec<_> = distinct.iter().take(4).collect();
        assert!(
            distinct.len() > 1,
            "frame has one colour only: {sample:?} (the window clears to {:?})",
            crate::ui::components::panel_palette::pane_bg(false),
        );
    }

    // Frame 2: same scene, warm pool — buffers are refilled, not
    // reallocated.  Same pixels, or the reuse is wrong.
    let v = view_of(&small, 1);
    renderer.render_layout_to_texture(&mut wr, &target, &layout, std::slice::from_ref(&v), &[], 0);
    let reused = texture_bytes_bgra(&target);
    assert_eq!(fresh, reused, "a refilled buffer must paint what a fresh one painted");

    // A bigger frame grows the buffers; going back to the small
    // one must not leave the big one's tail behind.  Instance
    // counts are what bound the draw, so a stale tail is exactly
    // the bug that would survive every other test here.
    let v = view_of(&big, 2);
    renderer.render_layout_to_texture(&mut wr, &target, &layout, std::slice::from_ref(&v), &[], 0);
    let full = texture_bytes_bgra(&target);
    assert_ne!(full, fresh, "the two scenes must actually differ, or this proves nothing");

    let v = view_of(&small, 1);
    renderer.render_layout_to_texture(&mut wr, &target, &layout, std::slice::from_ref(&v), &[], 0);
    let after_shrink = texture_bytes_bgra(&target);
    assert_eq!(
        fresh, after_shrink,
        "a smaller frame after a larger one must not inherit its leftovers",
    );
}

/// Two windows must not refill each other's instance buffers.
///
/// A frame is committed and left running now, so the instances
/// window A's frame is reading are still live when window B is
/// built.  One pool for every window meant B's fill overwrote them
/// and A's in-flight frame drew B's content — at B's coordinates,
/// so in A's top-left corner, for one frame.  Reported 2026-09-27
/// as a second window appearing in the corner of the first and
/// vanishing again.
///
/// Asserted on the buffers rather than on pixels on purpose: which
/// of the two frames the GPU finishes first is a race, so a pixel
/// test would pass on a good day with the bug still in.
#[test]
fn each_window_fills_its_own_instance_buffers() {
    use crate::grid::{Cell, Grid};
    use crate::layout::Layout;

    let mut renderer = match MetalRenderer::new_headless() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            return;
        }
    };
    let device = renderer.device().to_owned();
    let target = |w: usize, h: usize| {
        let desc = unsafe {
            objc2_metal::MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                TARGET_FORMAT, w, h, false,
            )
        };
        desc.setUsage(objc2_metal::MTLTextureUsage::RenderTarget);
        device.newTextureWithDescriptor(&desc)
    };
    let (Some(target_a), Some(target_b)) = (target(320, 160), target(200, 120)) else {
        return;
    };
    let (cell_w, cell_h) = renderer.cell_dims();
    let mut grid = Grid::new(20, 6);
    grid.set_cell(0, 0, Cell { ch: 'x', attrs: Default::default() });
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
        right_badge: "", agent_tui: false, cwd: "",
        top_fixed_h_cells: 0,
        bot_fixed_h_cells: 0,
        highlight_spans: &[],
        search_overlay: None,
        history: Default::default(), session: 0, strip: None, notice: None,
        seq: 1,
    };

    let mut wr_a = WindowRender::new();
    let mut wr_b = WindowRender::new();
    let layout_a = Layout::build(320.0, 160.0, 0.0, 0.0, 0.0, 1, 1, cell_w, cell_h);
    let layout_b = Layout::build(200.0, 120.0, 0.0, 0.0, 0.0, 1, 1, cell_w, cell_h);
    renderer.render_layout_to_texture(
        &mut wr_a, &target_a, &layout_a, std::slice::from_ref(&view), &[], 0,
    );
    renderer.render_layout_to_texture(
        &mut wr_b, &target_b, &layout_b, std::slice::from_ref(&view), &[], 0,
    );

    let (a, b) = (wr_a.instance_pool.memory(), wr_b.instance_pool.memory());
    assert!(!a.is_empty() && !b.is_empty(), "a window filled no buffer — nothing was compared");
    for ptr in &a {
        assert!(!b.contains(ptr), "both windows draw from the same memory");
    }
}
