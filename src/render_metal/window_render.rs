//! What the renderer keeps per window, and where one frame spent its time.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{MTLCommandBuffer, MTLCommandBufferStatus};

use super::InstanceBufferPool;
use crate::font_cache::FontCache;
use crate::frame_build::chrome::ChromeShape;
use crate::frame_build::frame::FrameKeep;
use crate::ui::components::chrome_target::ChromeTarget;

/// Renderer state that belongs to one window rather than to the
/// renderer as a whole.
///
/// RFC-005 — one `MetalRenderer` draws every window, one after the
/// other, so almost everything on it is legitimately shared: the
/// device, the pipelines, the font cache, both glyph atlases and every
/// scratch buffer (a scratch buffer is only live inside a single
/// `render_*` call).  Sharing them is the whole reason a second window
/// costs a layout and a render target rather than another copy of the
/// atlas.
///
/// Two things genuinely cannot be shared, and they live here:
///
/// * `kept.panes` is indexed by a window's pane order, so window A's
///   slot 0 and window B's slot 0 are different panes.
/// * `clear_bg_required` answers "does *this* window still owe a full
///   clear", which a resize of some other window must not satisfy.
///
/// Note what is deliberately absent: a per-scale glyph atlas.  Glyphs
/// are rasterised once at `FONT_POINT` and the atlas key carries the
/// quantised size, so one atlas already serves windows on displays of
/// different backing scales.
/// Where one IOSurface frame spent its time.
///
/// A frame that took 41 seconds is not a finding — "render is slow"
/// cannot be attacked.  These three numbers can: they separate CPU
/// instance-building (which includes rasterising every glyph the
/// atlas has never seen) from command encoding from the GPU wait, and
/// they are bracketed by calls the compiler cannot reorder across.
/// `glyphs_rasterised` is the companion witness — a large `build_us`
/// with a large glyph count is a cold atlas; the same `build_us` with
/// none is something else entirely, and without the count the two
/// look identical.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderSplit {
    pub build_us: u64,
    /// Blocked inside `queue.commandBuffer()`.  Split out from
    /// `encode_us` because the first real-workload numbers put
    /// 96–396 ms in "encode" — a phase that only assembles a command
    /// buffer on the CPU and has no business taking that long — and
    /// "encode" as one number cannot say whether the time went into
    /// *obtaining* the buffer or *filling* it.
    pub cmdbuf_us: u64,
    /// Filling the buffer: the four render passes.
    pub encode_us: u64,
    /// Of `encode_us`, the part spent allocating fresh per-frame
    /// instance buffers, and how many bytes that was.
    pub instbuf_us: u64,
    pub instbuf_bytes: u64,
    /// Bytes copied into this window's instance buffers for the frame.
    pub upload_bytes: u64,
    /// The context menu's pass, encoded
    /// after the others.  Zero unless one of them is open — which is
    /// itself worth knowing when a frame goes long.
    pub canvas_us: u64,
    pub gpu_wait_us: u64,
    /// What the GPU itself reports it spent executing this frame
    /// (`GPUEndTime - GPUStartTime`), against `gpu_wait_us`'s wall
    /// clock.  A large wait with a small exec means we were queued or
    /// descheduled, not that the work is heavy — and those two want
    /// completely different fixes.
    pub gpu_exec_us: u64,
    /// Of `build_us`, the per-pane loop vs everything else (chrome,
    /// sidebar, panels, modals, overlays).
    pub build_panes_us: u64,
    /// Panes that missed the per-pane instance cache and were rebuilt
    /// this frame, out of how many were considered.  A frame that
    /// rebuilds all of them every time is a cache that isn't working,
    /// which no timing on its own would reveal.
    pub panes_rebuilt: u32,
    pub panes_total: u32,
    /// In the panes that were rebuilt: the rows laid out again, out
    /// of how many those panes have.
    pub rows_rebuilt: u32,
    pub rows_total: u32,
    pub glyphs_rasterised: u32,
    /// Shelf evictions and whole-atlas rebuilds *during this frame*.
    /// Non-zero means the frame's own working set did not fit, so it
    /// threw away glyphs it went on to need again — a different
    /// failure from "there were simply a lot of new glyphs", and the
    /// two are indistinguishable from timing alone.
    pub evictions: u64,
    pub rebuilds: u64,
}

impl RenderSplit {
    /// `build=..ms encode=..ms gpu=..ms glyphs=..` — one field for a
    /// log line, so the three numbers always travel together.
    pub fn summary(&self) -> String {
        format!(
            "build_{:.1}ms(panes_{:.1}ms_{}/{}rebuilt) cmdbuf_{:.1}ms encode_{:.1}ms(instbuf_{:.1}ms/{:.1}MB) canvas_{:.1}ms gpu_{:.1}ms(exec_{:.1}ms) glyphs_{}",
            self.build_us as f64 / 1000.0,
            self.build_panes_us as f64 / 1000.0,
            self.panes_rebuilt,
            self.panes_total,
            self.cmdbuf_us as f64 / 1000.0,
            self.encode_us as f64 / 1000.0,
            self.instbuf_us as f64 / 1000.0,
            self.instbuf_bytes as f64 / 1048576.0,
            self.canvas_us as f64 / 1000.0,
            self.gpu_wait_us as f64 / 1000.0,
            self.gpu_exec_us as f64 / 1000.0,
            self.glyphs_rasterised,
        ) + &if self.evictions > 0 || self.rebuilds > 0 {
            format!(" evict_{} rebuild_{}", self.evictions, self.rebuilds)
        } else {
            String::new()
        }
    }
}

pub struct WindowRender {
    pub(super) kept: FrameKeep,
    /// Should the next render to an IOSurface target start with a
    /// hard Clear, or load the previous frame's pixels?  Clear is
    /// only ever needed when the SHAPE of what gets painted changes
    /// (layout mode switch, sidebar toggle, resize, first frame
    /// after attach) — in steady state the BG region is identical
    /// across frames, so loading preserves visually-identical
    /// content.  Crucially, Clear opens a cross-process race window:
    /// shell's presenter reads the IOSurface texture from a DIFFERENT
    /// process / queue, with no MTLSharedEvent fence to gate the
    /// read.  If presenter samples between the Clear and the cell
    /// draws on the GPU, it sees a uniform-BG texture — exactly
    /// what surfaces in the wild as "all 9 panes' contents momentarily
    /// disappear to background and reappear, no clear trigger,
    /// frequent" (the marspot flash bug, 2026-06-15).  Defaulting to
    /// Load eliminates the bg-only intermediate state.
    ///
    /// The flag itself now lives on `WindowRender`: it answers "does
    /// this window still owe a clear", and one window's resize must
    /// not discharge another's.
    clear_bg_required: bool,
    /// The frame this window has on the GPU, if any.
    ///
    /// The live path used to end each frame in `waitUntilCompleted`,
    /// on the main loop.  That is one thread for input, PTY pumping,
    /// every pane and the GPU round-trip, so a slow round-trip stops
    /// the whole terminal: measured across 166 stalls on a working
    /// machine, the average wait was **374 ms against 3.3 ms of actual
    /// GPU execution**, the worst 3.6 s — all of it with keystrokes
    /// queueing up behind it (2026-09-06, "在我们这开 codex，输入有时
    /// 候都会卡，在 iTerm2 很流畅").  The GPU was not busy; we were
    /// queued behind a loaded machine's other work and chose to block.
    ///
    /// So the frame is committed and left running.  Its buffer is kept
    /// here and polled — `settled()` — and only when it reports
    /// `Completed` does the surface flip and `SurfaceReady` go out, so
    /// the shell still only ever samples a finished surface.  A window
    /// with a frame in flight simply does not start another; the loop
    /// goes back to reading input, which is the whole point.
    ///
    /// One frame in flight, never two — per window, which is the part
    /// that needed saying: the two surfaces still alternate with a full
    /// frame between reuses, and the instance pool may still be
    /// refilled in place, but only because the pool below is this
    /// window's own.  While the frame was awaited, one pool for all
    /// windows was sound; it stopped being sound here.
    pub(super) in_flight: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
    /// This window's per-pass instance buffers, refilled frame to
    /// frame on the IOSurface path.  See [`InstanceBufferPool`].
    ///
    /// Per window, because the frame that reads them outlives the call
    /// that fills them and `in_flight` is per window too: one pool
    /// shared by all windows meant window B's fill overwrote the
    /// instances window A's in-flight frame was still reading, and
    /// window A drew window B's content — at window B's coordinates,
    /// so in window A's top-left corner, for one frame (2026-09-27
    /// report: "window2 会被显示到 window1 左上角，又消失").
    pub(super) instance_pool: InstanceBufferPool,
    /// The GPU's own account of the last completed frame.
    pub(super) last_gpu_exec_us: u64,
}

impl Default for WindowRender {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowRender {
    pub fn new() -> Self {
        // Starts true: a window has never been painted, so its first
        // frame owes a full clear.
        Self {
            kept: FrameKeep::default(),
            clear_bg_required: true,
            in_flight: None,
            instance_pool: InstanceBufferPool::default(),
            last_gpu_exec_us: 0,
        }
    }

    /// What `p` is on in the window's chrome: its toolbar, sidebar
    /// and title strips, laid out first as the next frame will draw
    /// them if `shape` says they have moved since the last.
    pub fn chrome_at(&mut self, font: &mut FontCache, shape: &ChromeShape<'_>, p: (f64, f64)) -> Option<ChromeTarget> {
        self.kept.chrome.at(font, shape, p)
    }

    /// What `p` is on in the chrome as the window last laid it out.
    pub fn chrome_hit_painted(&self, p: (f64, f64)) -> Option<ChromeTarget> {
        self.kept.chrome.hit(p)
    }

    /// The pointer moved to `p` over the window: true when what lights
    /// in the chrome changed, a frame to paint and nothing to lay out.
    pub fn chrome_pointer_move(&mut self, font: &mut FontCache, shape: &ChromeShape<'_>, p: (f64, f64)) -> bool {
        self.kept.chrome.pointer_move(font, shape, p)
    }

    /// The pointer is no longer over the window's chrome.
    pub fn chrome_pointer_leave(&mut self) -> bool {
        self.kept.chrome.pointer_leave()
    }

    /// A press at `p`, remembered for the release; what it is on.
    pub fn chrome_press(&mut self, font: &mut FontCache, shape: &ChromeShape<'_>, p: (f64, f64)) -> Option<ChromeTarget> {
        self.kept.chrome.press(font, shape, p)
    }

    /// The release at `p` (`None`: in another window or off every
    /// window): the control pressed, if it is let go on.
    pub fn chrome_release(
        &mut self,
        font: &mut FontCache,
        shape: &ChromeShape<'_>,
        p: Option<(f64, f64)>,
    ) -> Option<ChromeTarget> {
        self.kept.chrome.release(font, shape, p)
    }

    /// Is a frame still on the GPU for this window?
    ///
    /// A caller that sees `true` must not start another frame — it
    /// would overwrite the instance buffers the GPU is reading.
    pub fn frame_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Has the in-flight frame finished?  Reaps it if so.
    ///
    /// Returns `true` exactly once per frame, on the poll that finds it
    /// complete — that is the moment the surface is safe to show.
    pub fn settled(&mut self) -> bool {
        let Some(cmd) = self.in_flight.as_ref() else {
            return false;
        };
        // `Error` counts as settled: the surface will not improve by
        // waiting, and leaving the window stuck with a frame that will
        // never complete would freeze it forever.
        let st = cmd.status();
        if st != MTLCommandBufferStatus::Completed && st != MTLCommandBufferStatus::Error {
            return false;
        }
        let (s, e) = (cmd.GPUStartTime(), cmd.GPUEndTime());
        self.last_gpu_exec_us = ((e - s).max(0.0) * 1e6) as u64;
        self.in_flight = None;
        true
    }

    /// GPU time of the last completed frame, in microseconds.
    pub fn last_gpu_exec_us(&self) -> u64 {
        self.last_gpu_exec_us
    }

    /// The next frame for this window must clear rather than load.
    /// Call from anywhere the visible BG region shape is about to
    /// change (layout mode switch, sidebar toggle, resize, surface
    /// reattach).  See the `clear_bg_required` field doc for the race
    /// that motivates the Load-default for steady-state frames.
    pub fn mark_bg_clear_required(&mut self) {
        self.clear_bg_required = true;
    }

    /// Consume the flag: returns whether this frame owes a clear, and
    /// leaves the window not owing one.  The render path uses it, and
    /// it is the only way to observe the flag — which is what lets a
    /// test prove one window's flag is not another's.
    pub fn take_clear_required(&mut self) -> bool {
        std::mem::take(&mut self.clear_bg_required)
    }
}
