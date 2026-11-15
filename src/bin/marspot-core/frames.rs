//! The frames the GPU has finished: shown to the shell, in order.

use marspot::latency_trace::{self as trace, Stage};
use marspot::lx_debug_sampled;
use marspot::shell_proto::{encode_caret_rect, encode_surface_ready, Frame, MsgType};
use marspot_term::frame_writer::FrameWriter;

use super::{send_to_shell, CoreApp, WindowState};
use marspot::iosurface::IOSurface;
use marspot::lx_error;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{MTLDevice, MTLTexture};
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether the shell shows our surfaces directly and reports each one
/// shown (`FramePace`).  Said once by a shell, for the life of the
/// connection, which is the life of this process.
static PACED: AtomicBool = AtomicBool::new(false);

pub(super) fn pace() {
    PACED.store(true, Ordering::Relaxed);
}

/// Where one window's frames land: the IOSurface pair L1 handed us
/// for it, the Metal textures wrapping them, and which half the next
/// frame writes into.  Windows are peers; each owns its own paint
/// target.
pub(super) struct WindowSurfaces {
    pair: [IOSurface; 2],
    tex: [Retained<ProtocolObject<dyn MTLTexture>>; 2],
    /// The half the next render writes; flipped after each frame so
    /// the shell is only ever pointed at a surface the GPU has finished.
    pub(super) writing_idx: usize,
    /// The surface last announced to a pacing shell and not yet
    /// reported shown.  While set, the other half may still be on the
    /// screen, and it is the half the next frame would be drawn into.
    unshown: Option<u32>,
}

impl WindowSurfaces {
    /// Look both ids up and wrap them in textures.  `None` when either
    /// lookup or texture creation fails — the caller keeps whatever
    /// pair the window already had, which is the difference between a
    /// dropped frame and a black window.
    pub(super) fn attach(front_id: u32, back_id: u32, device: &ProtocolObject<dyn MTLDevice>) -> Option<Self> {
        let front = IOSurface::lookup(front_id)?;
        let back = IOSurface::lookup(back_id)?;
        match (front.make_metal_texture(device), back.make_metal_texture(device)) {
            (Ok(tf), Ok(tb)) => Some(Self { pair: [front, back], tex: [tf, tb], writing_idx: 0, unshown: None }),
            (tf, tb) => {
                if let Err(e) = tf {
                    lx_error!("core.attach.metal_texture_front_failed", &format!("{e:?}"));
                }
                if let Err(e) = tb {
                    lx_error!("core.attach.metal_texture_back_failed", &format!("{e:?}"));
                }
                None
            }
        }
    }

    pub(super) fn ids(&self) -> (u32, u32) {
        (self.pair[0].id(), self.pair[1].id())
    }

    pub(super) fn writing_tex(&self) -> Retained<ProtocolObject<dyn MTLTexture>> {
        self.tex[self.writing_idx].clone()
    }

    pub(super) fn writing_surface_id(&self) -> u32 {
        self.pair[self.writing_idx].id()
    }

    /// The half just finished is announced: the next frame writes the
    /// other, once a pacing shell says this one is shown.
    pub(super) fn announce(&mut self) -> u32 {
        let id = self.writing_surface_id();
        self.writing_idx = 1 - self.writing_idx;
        if PACED.load(Ordering::Relaxed) {
            self.unshown = Some(id);
        }
        id
    }
}

/// Whether the window's next frame has to wait for the shell to report
/// its last one shown.
pub(super) fn waits(w: &WindowState) -> bool {
    w.surfaces.as_ref().is_some_and(|s| s.unshown.is_some())
}

/// The shell put `surface` on `window_id`'s window a refresh ago.  A
/// report naming any other surface than the one last announced is
/// about a frame since replaced, and changes nothing.
pub(super) fn shown(app: &mut CoreApp, window_id: u32, surface: u32) {
    let Some(s) = app.windows.iter_mut().find(|w| w.window_id == window_id).and_then(|w| w.surfaces.as_mut()) else {
        return;
    };
    if s.unshown == Some(surface) {
        s.unshown = None;
        trace::mark(Stage::FrameShown, 0);
    }
}

/// Window `wi` was handed a pair and drew its first frame into it:
/// tell the shell, which installs the pair on that.
pub(super) fn announce_attached(app: &mut CoreApp, wi: usize, control_writer: &FrameWriter) {
    if let Some(s) = app.windows[wi].surfaces.as_mut() {
        let ack = Frame::new(MsgType::SurfaceReady, encode_surface_ready(s.announce()));
        send_to_shell(control_writer, ack, "surface_ready");
    }
}

/// Flip every window whose frame finished since the last pass onto
/// it, and tell the shell.  Only a finished surface is ever named, so
/// the shell never samples one still being drawn.
pub(super) fn announce_settled(app: &mut CoreApp, control_writer: &FrameWriter, frame: u64) {
    for wi in 0..app.windows.len() {
        if !app.windows[wi].render.settled() {
            continue;
        }
        if trace::enabled() {
            trace::mark(Stage::Settled, wi as u32);
            trace::mark_at(Stage::GpuExecUs, app.windows[wi].render.last_gpu_exec_us() as u32, 0);
        }
        let caret = app.windows[wi].pending_caret.take();
        let (surface_id, writing_idx) = {
            let Some(s) = app.windows[wi].surfaces.as_mut() else { continue };
            let idx = s.writing_idx;
            (s.announce(), idx)
        };
        // Sampled per-frame DEBUG.  1/8 keeps a ~7-Hz heartbeat on
        // a busy display (60 Hz cap) without flooding when the
        // user runs `MARSPOT_LOG_CORE=debug` to investigate latency
        // / dropped frames.  surface_id ties it to the IOSurface
        // lifecycle on the shell side; dur_us is the actual GPU
        // wait, the dominant time cost in a frame.
        lx_debug_sampled!(
            "render.frame",
            8,
            "frame rendered",
            frame = frame,
            window_id = app.windows[wi].window_id,
            writing_idx = writing_idx,
            surface_id = surface_id,
            dur_us = app.windows[wi]
                .last_render_at
                .map(|t| t.elapsed().as_micros() as u64)
                .unwrap_or(0),
            n_panes = app.windows[wi].panes.len(),
            focused_idx = app.windows[wi].focused_idx
        );
        // Per-frame ack — the just-completed surface ID.  In v=2
        // this replaces the empty-payload `FrameRendered` poke:
        // shell uses the id to flip its presenter's `current_idx`,
        // then presents.  Same one frame round-trip the old path
        // had, but the present now samples a guaranteed-finished
        // surface instead of racing the writing one.  The shell
        // resolves which window an id belongs to by looking it up
        // in its per-window pairs, so no window tag is needed.
        let ack = Frame::new(MsgType::SurfaceReady, encode_surface_ready(surface_id));
        send_to_shell(control_writer, ack, "surface_ready");
        trace::mark(Stage::SurfaceSent, wi as u32);
        // the wake a shell from before `SurfaceReady` listens for; a
        // pacing shell is not one
        if !PACED.load(Ordering::Relaxed) {
            let fr = Frame::new(MsgType::FrameRendered, Vec::new());
            send_to_shell(control_writer, fr, "frame_rendered");
        }
        // Publish the focused-pane caret so the shell can anchor
        // the IME candidate window.  Dedupe — an idle cursor must
        // not stream identical frames at render cadence.
        if app.windows[wi].last_caret_sent != Some(caret) {
            app.windows[wi].last_caret_sent = Some(caret);
            let f = Frame::new(
                MsgType::CaretRect,
                encode_caret_rect(caret, app.windows[wi].window_id),
            );
            send_to_shell(control_writer, f, "caret_rect");
        }
    }
}

/// What the frame just committed for window `wi` cost, for a build
/// that times frames.
pub(super) fn trace_committed(wi: usize, split: marspot::render_metal::RenderSplit) {
    if !trace::enabled() {
        return;
    }
    trace::mark(Stage::RenderCommitted, wi as u32);
    trace::mark_at(Stage::BuildUs, split.build_us as u32, 0);
    trace::mark_at(Stage::PanesUs, split.build_panes_us as u32, 0);
    trace::mark_at(Stage::EncodeUs, (split.cmdbuf_us + split.encode_us) as u32, 0);
    trace::mark_at(Stage::UploadBytes, split.upload_bytes as u32, 0);
    trace::mark_at(Stage::RowsRebuilt, split.rows_rebuilt, 0);
    trace::mark_at(Stage::RowsTotal, split.rows_total, 0);
}
