//! The shell's state for one native window, and what a resize of it
//! is worth.

use super::*;

/// Everything the shell owns on behalf of ONE native window.
///
/// RFC-005 — these used to sit directly on `ShellApp`, which is how
/// "there is exactly one window" was encoded on the L1 side.  The core
/// stays single (one process drives every window); what multiplies is
/// the window-shaped kernel state: a surface pair, the presenter that
/// samples it, and the gates that say whether this window has anything
/// worth showing yet.
pub(super) struct ShellWindow {
    /// Matches the id the view/delegate tag their events with, and the
    /// id carried on the wire.
    pub(super) window_id: u32,
    /// Which entry of `window-state.bin` this window's geometry lives
    /// in — and, by construction, which entry of `shell-state.bin`'s
    /// window list holds its panes (the core keeps the same number in
    /// `WindowState::frame_index`).
    ///
    /// Fixed for the window's life, deliberately not its position in
    /// `self.windows`: writing frames by position meant closing a
    /// window shifted every later window down a slot, so the survivors
    /// wrote over the closed window's geometry and inherited each
    /// other's on the next launch.
    pub(super) frame_index: usize,
    /// The pair the core is drawing: one half is on the window.
    pub(super) surfaces: Option<SurfacePair>,
    /// Created in `resized` (or `restart_core`) and not yet promoted.
    /// Once the core confirms via `SurfaceReady(id)` matching either
    /// id of this pair, we install it as the live pair and let the
    /// presenter swap to the ready id.  A later resize replaces the
    /// pending entry; the dropped one is abandoned (`release`).
    pub(super) pending_surfaces: Option<SurfacePair>,
    /// What puts a surface on this window's view.
    pub(super) layers: Option<FrameLayers>,
    /// True after the core has confirmed at least one SurfaceReady for
    /// THIS window.  Until then `redraw` skips `present()` so the user
    /// sees the NSWindow's BG colour (the terminal's) instead of an
    /// unfilled black IOSurface — kills the cold-start flash.
    pub(super) first_frame_ready: bool,
    /// `redraw()` only calls `present()` when this is true.  Set by
    /// every `FrameRendered` poke from the active core (its
    /// per-frame ack that the IOSurface has been fully written +
    /// waitUntilCompleted'd).  Cleared by `redraw` after `present()`.
    ///
    /// Why: the redraw pump (`start_redraw_pump`) wakes the AppKit
    /// redraw callback unconditionally — if the callback then samples
    /// the IOSurface, it can land MID-RENDER (between core's BG pass
    /// and FG glyph pass), producing the frequent "all 9 panes'
    /// contents momentarily disappear and reappear" flash documented in
    /// the 2026-06-15 debugging.  So a window is presented only when
    /// this is set: a new frame, or a banner change (`refresh_banner`),
    /// which is drawn by the presenter and has no frame of its own.
    /// It stays set when `present()` could not draw (no drawable), so
    /// the next wake tries again.  A poke the main thread missed is not
    /// lost either: every wake drains the core's inbox first.
    ///
    /// There used to be a second net here, a present every 5 s with
    /// nothing pending, for "core froze mid-render and no poke is
    /// coming".  A frozen core is the PONG deadline's to catch, and
    /// presenting the same surface again shows nothing new; what that
    /// net really covered was the skipped present and the banner, both
    /// handled above.  Each spurious present made WindowServer
    /// composite again.
    pub(super) frame_pending: bool,
    /// Whether this window has been presented once, for the one-line
    /// `WINDOW_FIRST_PRESENT` log.
    pub(super) presented: bool,
    /// Dev seam only: this window is to be closed again as soon as it
    /// has painted (`MARSPOT_DEV_CLOSE_EXTRA`).  Latched so the close
    /// runs once.
    pub(super) dev_close_after_paint: bool,
    /// F3+6.1 — last saved (x, y, w, h, display_id) so per-frame
    /// `resized` / `moved` callbacks dedup: only the saves where the
    /// frame actually changed since the last bin write actually fire
    /// I/O.  Eliminates the live-resize 60+ writes/s storm.
    pub(super) last_saved_window: Option<(f64, f64, f64, f64, u32)>,
    /// What the last `SurfaceAttach` for this window described.  A
    /// resize notification carrying none of it new must not allocate a
    /// pair: AppKit sends streams of them (2026-09-27, ~144/s at the
    /// display rate for a window nobody was resizing), and each pair
    /// costs two IOSurfaces here, two Metal textures in the core, and
    /// a presenter swap onto a surface the core has only just started
    /// painting — which is the flicker that was reported.
    pub(super) last_attached: Option<AttachedGeometry>,
    /// Resize notifications skipped since the last real one, counted so
    /// the log says how many arrived without saying it 144 times a
    /// second.
    pub(super) skipped_resizes: u32,
}

/// What a `SurfaceAttach` told the core about a window.
#[derive(Clone, Copy)]
pub(super) struct AttachedGeometry {
    pub(super) w_px: usize,
    pub(super) h_px: usize,
    pub(super) scale: f64,
    pub(super) lights_right_phys: f64,
}

impl AttachedGeometry {
    /// Does this describe the same pixels as `other`?  The chrome
    /// measurement is deliberately excluded: it travels in a frame of
    /// its own that costs no surfaces.
    fn same_pixels(&self, other: &Self) -> bool {
        self.w_px == other.w_px && self.h_px == other.h_px && self.scale == other.scale
    }
}

/// What a resize notification is actually worth.
#[derive(Debug, PartialEq)]
pub(super) enum ResizeWork {
    /// The pixels moved: a fresh pair, and the core is told.
    Attach,
    /// Only the window-button cluster moved (a full-screen
    /// transition): one small frame, no surfaces.
    ChromeOnly,
    /// Nothing in it is new.
    Nothing,
}

/// Decide it from the last attach alone, so the rule is one place and
/// testable away from AppKit.
pub(super) fn resize_work(prev: Option<AttachedGeometry>, now: &AttachedGeometry) -> ResizeWork {
    let Some(prev) = prev else { return ResizeWork::Attach };
    if !prev.same_pixels(now) {
        return ResizeWork::Attach;
    }
    if (prev.lights_right_phys - now.lights_right_phys).abs() > 0.5 {
        return ResizeWork::ChromeOnly;
    }
    ResizeWork::Nothing
}

#[cfg(test)]
mod resize_work_tests;

impl ShellWindow {
    pub(super) fn new(window_id: u32, frame_index: usize) -> Self {
        Self {
            window_id,
            frame_index,
            surfaces: None,
            pending_surfaces: None,
            layers: None,
            first_frame_ready: false,
            frame_pending: false,
            dev_close_after_paint: false,
            presented: false,
            last_saved_window: None,
            last_attached: None,
            skipped_resizes: 0,
        }
    }
}
