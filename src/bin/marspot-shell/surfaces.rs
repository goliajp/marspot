//! The surfaces a window's frames arrive in, and putting the one the
//! core last finished on the window.

use marspot::iosurface::IOSurface;
use marspot::{lx_debug, lx_debug_sampled, lx_error, lx_event, lx_info, lx_warn};

use crate::ShellApp;

/// Two surfaces of one size.  The core draws them in turn and names
/// each as it finishes (`SurfaceReady`), so the one on the window is
/// never the one being drawn.
pub(crate) struct SurfacePair {
    pub(crate) front: IOSurface,
    pub(crate) back: IOSurface,
}

impl SurfacePair {
    pub(crate) fn create(w_px: usize, h_px: usize) -> Result<Self, String> {
        let front = IOSurface::create(w_px, h_px)?;
        let back = IOSurface::create(w_px, h_px)?;
        front.tag_srgb();
        back.tag_srgb();
        // which surfaces a frame was shown from is the first question
        // of every flash or black-window report
        lx_debug!(
            "shell.surface.create_pair",
            "IOSurface pair created",
            front_id = front.id(),
            back_id = back.id(),
            w_px = w_px,
            h_px = h_px
        );
        Ok(Self { front, back })
    }

    pub(crate) fn ids(&self) -> (u32, u32) {
        (self.front.id(), self.back.id())
    }

    pub(crate) fn width(&self) -> usize {
        self.front.width()
    }

    pub(crate) fn height(&self) -> usize {
        self.front.height()
    }

    fn holds(&self, id: u32) -> bool {
        self.front.id() == id || self.back.id() == id
    }

    fn surface(&self, id: u32) -> &IOSurface {
        if self.front.id() == id { &self.front } else { &self.back }
    }

    /// The shell is done with the pair.
    pub(crate) fn release(self) {
        lx_debug!(
            "shell.surface.release_pair",
            "IOSurface pair released",
            front_id = self.front.id(),
            back_id = self.back.id()
        );
    }
}

impl ShellApp {
    /// A new pair for window `wi`, waiting for the core's first frame
    /// in it; one that was still waiting is dropped.  The pair's ids.
    pub(crate) fn fresh_pending_pair(&mut self, wi: usize, w_px: usize, h_px: usize) -> Result<(u32, u32), String> {
        let pair = SurfacePair::create(w_px, h_px)?;
        if let Some(stale) = self.windows[wi].pending_surfaces.take() {
            stale.release();
        }
        let ids = pair.ids();
        self.windows[wi].pending_surfaces = Some(pair);
        Ok(ids)
    }

    /// The core finished a frame in surface `id`.  Two cases:
    ///
    /// 1. The id is in the window's live pair: the frame goes up.
    /// 2. The id is in its pending pair (a resize or a new core made
    ///    one and the core has now drawn in it): that pair becomes the
    ///    live one, and the frame goes up.
    ///
    /// An id in neither is a late word about a pair since replaced.
    pub(crate) fn on_surface_ready(&mut self, id: u32) {
        // how a frame is shown is settled by the core's HelloAck
        if let Some(c) = self.active.as_mut()
            && !c.hello_acked
        {
            c.early_ready.push(id);
            return;
        }
        // Surface ids are global, so the pair holding this one names
        // its window — no window id is needed on the wire.
        let Some(i) = self.window_index_of_surface(id) else {
            lx_warn!("shell.surface_ready.id_unknown", "SurfaceReady ignored — id belongs to no window's pair", id = id);
            return;
        };
        if self.windows[i].pending_surfaces.as_ref().is_some_and(|p| p.holds(id)) {
            let new_pair = self.windows[i].pending_surfaces.take().expect("checked above");
            lx_info!(
                "shell.presenter.pair_swapped",
                "window now showing a new pair",
                window_id = self.windows[i].window_id,
                front = new_pair.front.id(),
                back = new_pair.back.id(),
                acked = id
            );
            if let Some(old) = self.windows[i].surfaces.replace(new_pair) {
                old.release();
            }
            self.put_up(i, id);
            self.dev_close_extra_if_armed(i);
        } else {
            // the dominant rate on a busy display: sampled
            lx_debug_sampled!("shell.surface.swap", 8, "per-frame SurfaceReady ack", id = id);
            self.put_up(i, id);
        }
    }

    /// Surface `id` of window `i`'s live pair is the frame to show.
    fn put_up(&mut self, i: usize, id: u32) {
        let paced = self.active.as_ref().is_some_and(|c| c.paced);
        let w = &mut self.windows[i];
        let (Some(pair), Some(layers)) = (w.surfaces.as_ref(), w.layers.as_mut()) else {
            // Silently skipping here is how the 2026-07-28 black
            // window stayed invisible in the logs.
            lx_error!(
                "shell.surface_ready.no_presenter",
                "frame ready but this window has nothing to show it with — it will render black",
                window_id = w.window_id,
                id = id
            );
            return;
        };
        if paced {
            layers.show(pair.surface(id));
            w.first_frame_ready = true;
            w.frame_pending = false;
            if !std::mem::replace(&mut w.presented, true) {
                lx_event!("WINDOW_FIRST_PRESENT", "first present for this window", window_id = w.window_id);
            }
            return;
        }
        // a core that cannot be told when a surface has left the
        // screen: its frames are copied, by a presenter made now
        let pointed = layers.legacy(&pair.front, &pair.back).and_then(|p| {
            if !p.swap_to_id(id) {
                p.set_pair(&pair.front, &pair.back)?;
                p.swap_to_id(id);
            }
            Ok(())
        });
        if let Err(e) = pointed {
            lx_error!("shell.set_pair_failed", &e);
            return;
        }
        w.first_frame_ready = true;
        w.frame_pending = true;
    }

    /// The core said how its frames are to be shown: show the ones it
    /// finished before saying so.
    pub(crate) fn show_early_frames(&mut self) {
        let Some(c) = self.active.as_mut() else { return };
        lx_event!("FRAME_PACE", "how this core's frames reach the window", paced = c.paced);
        for id in std::mem::take(&mut c.early_ready) {
            self.on_surface_ready(id);
        }
    }

    /// Copy every window that owes a frame and has a copying
    /// presenter, not only the one a callback happens to be about:
    /// wakes are dispatched to the first window, and a duty keyed to
    /// the callback's window ran for that one alone (2026-07-28, a
    /// second window that stayed black).
    pub(crate) fn present_windows(&mut self) {
        for w in self.windows.iter_mut() {
            // Nothing before the core has drawn into this window's
            // pair: an unfilled surface would flash before the content.
            if !w.first_frame_ready || !w.frame_pending {
                continue;
            }
            let Some(layers) = w.layers.as_mut() else { continue };
            // a window shown from its layer has nothing to copy
            if let Some(p) = layers.copying() {
                // no drawable: the frame stays owed and the clock tries again
                if !p.present() {
                    continue;
                }
                layers.copied();
                if !std::mem::replace(&mut w.presented, true) {
                    lx_event!("WINDOW_FIRST_PRESENT", "first present for this window", window_id = w.window_id);
                }
            }
            w.frame_pending = false;
        }
    }

    /// What a sandbox or a measuring build does on every pass.
    pub(crate) fn dev_pass(&mut self) {
        if self.windows.iter().any(|w| w.presented) {
            crate::dev_window::pass();
        }
        #[cfg(feature = "latency-trace")]
        for w in self.windows.iter_mut() {
            if let Some(l) = w.layers.as_mut() {
                l.check_screen();
            }
        }
    }
}
