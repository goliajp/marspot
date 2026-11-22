//! Opening, finding and closing the shell's native windows, and handing
//! each one's surfaces to the core.

use super::*;

impl ShellApp {
    /// Open another native window (Cmd-N).
    ///
    /// RFC-005 semantics: a fresh window starts as a 1×1 grid with one
    /// new pane and becomes key.  L1 allocates the id — it is what
    /// every input frame carries and what the core keys a
    /// `WindowState` off.
    ///
    /// Only *asks* here.  The window is created after this dispatch
    /// returns (AppKit calls made while the app-state borrow is live
    /// panic on re-entry), and `window_opened` finishes the job.
    pub(super) fn open_new_window(&mut self) {
        // No saved frame: a new window is centred, not restored.
        self.open_window_with_frame(None, None);
    }

    /// Close the gap a window discarded for good left behind: drop its
    /// geometry and move every slot above it down one.
    ///
    /// A window that merely *closed* keeps its slot — it is parked and
    /// coming back.  Only a window whose panes the user closed is
    /// forgotten, and then both saved lists have to shorten together
    /// or the windows above it inherit each other's frames.  The core
    /// runs the same compaction on its half (`CoreApp::forget_slot`).
    fn forget_slot(&mut self, slot: usize) {
        if slot < self.saved_window_frames.len() {
            self.saved_window_frames.remove(slot);
        }
        for w in &mut self.windows {
            if w.frame_index > slot {
                w.frame_index -= 1;
            }
        }
        for s in self.pending_frame_index.values_mut() {
            if *s > slot {
                *s -= 1;
            }
        }
        self.save_window_frames();
    }

    /// The slot a brand-new window takes: one past every slot already
    /// spoken for, on disk or in flight.  Monotonic, so a window the
    /// user opens now can never take the geometry entry of a window
    /// that is merely closed and waiting to be restored.
    pub(super) fn next_frame_index(&self) -> usize {
        let live = self.windows.iter().map(|w| w.frame_index);
        let pending = self.pending_frame_index.values().copied();
        live.chain(pending)
            .chain(std::iter::once(
                self.saved_window_frames.len().saturating_sub(1),
            ))
            .max()
            .map(|m| m + 1)
            .unwrap_or(0)
    }

    /// RFC-005 step 6b — reopen a window the last session had, at the
    /// geometry entry `frame_index` of `window-state.bin`.
    ///
    /// The core drives this: it read the layout file and knows how
    /// many windows there were, while L1 owns the windows and their
    /// ids.  An index past the end of the geometry file (the two files
    /// disagreeing after a crash between writes) is not an error —
    /// the window opens at the default rect and the panes still land.
    pub(super) fn restore_window(&mut self, frame_index: u32) {
        // Only the FIRST core of this launch restores windows.  A
        // replacement core (silent update, crash respawn) reads the
        // same saved file and asks again — honouring that would stack
        // a second copy of every window on top of the ones already
        // open.  L1 owns windows, so L1 is where that judgement
        // belongs; the core cannot tell whether it is the first one.
        //
        // Keyed on the core generation rather than on "have we
        // restored yet", because a cold launch sends the whole batch
        // in one burst, before any of those windows exist.
        if self.safe_mode {
            lx_warn!(
                "shell.window.restore_refused_safe_mode",
                "safe-mode boot restores no extra windows",
                frame_index = frame_index
            );
            return;
        }
        if self.core_generation > 1 {
            lx_event!(
                "WINDOW_RESTORE_IGNORED",
                "not a cold launch; the windows are already open",
                frame_index = frame_index,
                windows = self.windows.len()
            );
            return;
        }
        let frame = marspot::state::read_windows()
            .and_then(|v| v.into_iter().nth(frame_index as usize))
            .filter(|w| w.w > 50.0 && w.h > 50.0)
            .map(|w| (w.x, w.y, w.w, w.h));
        if frame.is_none() {
            lx_warn!(
                "shell.window.restore_no_frame",
                "no saved geometry for this window; opening at default rect",
                frame_index = frame_index
            );
        }
        self.open_window_with_frame(frame, Some(frame_index as usize));
        lx_event!(
            "WINDOW_RESTORE",
            "core asked for a saved window to be reopened",
            frame_index = frame_index
        );
    }

    /// Allocate an id and ask AppKit for the window.
    ///
    /// Only *asks*.  The window is created after this dispatch returns
    /// (AppKit calls made while the app-state borrow is live panic on
    /// re-entry), and `window_opened` finishes the job.
    pub(super) fn open_window_with_frame(
        &mut self,
        frame_pt: Option<(f64, f64, f64, f64)>,
        frame_index: Option<usize>,
    ) {
        let window_id = self.next_window_id;
        self.next_window_id = self.next_window_id.wrapping_add(1).max(1);
        let slot = frame_index.unwrap_or_else(|| self.next_frame_index());
        self.pending_frame_index.insert(window_id, slot);
        let attrs = WindowAttrs {
            title: DEFAULT_TITLE.to_string(),
            width_logical: DEFAULT_W_PT,
            height_logical: DEFAULT_H_PT,
            frame_pt,
            bg: marspot::system_theme::startup_palette().bg,
        };
        marspot::app::open_window(window_id, &attrs);
    }

    /// Announce every window past the boot one to a core that has just
    /// been spawned.
    ///
    /// A core learns about the boot window from the env pair it is
    /// spawned with, and about later windows from the
    /// `SurfaceAttachWindow` sent when each opened.  A *replacement*
    /// core (silent update, crash respawn) missed all of those: it
    /// would come up knowing one window, leaving every other window
    /// painting-less forever while its panes were adopted as orphans
    /// into the first one.  Replaying the attaches is what makes a core
    /// swap invisible with more than one window open.
    pub(super) fn announce_windows_to_new_core(&mut self, ctx: &MarspotAppCtx) {
        let scale = ctx.scale();
        let lights = ctx.traffic_lights_right_phys();
        for i in 1..self.windows.len() {
            let Some(s) = self.windows[i].surfaces.as_ref() else { continue };
            let (w_px, h_px) = (s.width(), s.height());
            // a pair of its own, as for the first window: the one on
            // the screen is not the new core's to paint
            let Ok((f, b)) = self.fresh_pending_pair(i, w_px, h_px) else { continue };
            let (w, h) = (w_px as f64, h_px as f64);
            // Replay into a fresh core: `ctx` names one window, and
            // its cluster measurement stands in for the rest.  Every
            // window's next resize / attach carries its own.
            self.send_surface_attach(
                self.windows[i].window_id, f, b, w, h, scale, lights,
            );
            lx_event!(
                "WINDOW_REANNOUNCED",
                "replayed a window's surface pair into the new core",
                window_id = self.windows[i].window_id
            );
        }
    }

    /// Index of the window carrying `window_id`.
    pub(super) fn window_index(&self, window_id: u32) -> Option<usize> {
        self.windows.iter().position(|w| w.window_id == window_id)
    }

    /// The window whose live or pending pair contains `surface_id`.
    ///
    /// This is why `SurfaceReady` never needed a window id on the
    /// wire: surface ids are global, so the pair that holds one names
    /// its window unambiguously.
    pub(super) fn window_index_of_surface(&self, surface_id: u32) -> Option<usize> {
        self.windows.iter().position(|w| {
            let has = |p: &Option<SurfacePair>| {
                p.as_ref().is_some_and(|p| {
                    let (f, b) = p.ids();
                    f == surface_id || b == surface_id
                })
            };
            has(&w.surfaces) || has(&w.pending_surfaces)
        })
    }

    /// Announce a surface pair for one window.
    ///
    /// Sends BOTH forms: the legacy window-blind `SurfaceAttach`, which
    /// is the only one a core predating RFC-005 understands, and the
    /// window-aware one.  A new core ignores the legacy frame from the
    /// first window-aware frame onward, so neither peer ever attaches
    /// twice and neither is left without a surface across a skewed
    /// update.  The legacy frame only makes sense for the first window
    /// — it carries no id — so later windows send just the new form.
    pub(super) fn send_surface_attach(
        &self,
        window_id: u32,
        f: u32,
        b: u32,
        w_phys: f64,
        h_phys: f64,
        scale: f64,
        lights_right_phys: f64,
    ) {
        if window_id == marspot::shell_proto::FIRST_WINDOW_ID {
            self.send(
                MsgType::SurfaceAttach,
                encode_surface_attach(f, b, w_phys, h_phys, scale),
            );
        }
        // The slot rides along so the core can match this window to
        // its saved record by name rather than by arrival order — see
        // `encode_surface_attach_window`.  A window we have not taken
        // on yet (this runs during the replay into a fresh core too)
        // falls back to 0, which the core reads as "no opinion".
        let slot = self
            .window_index(window_id)
            .map(|i| self.windows[i].frame_index)
            .or_else(|| self.pending_frame_index.get(&window_id).copied())
            .unwrap_or(0) as u32;
        self.send(
            MsgType::SurfaceAttachWindow,
            encode_surface_attach_window(
                f, b, w_phys, h_phys, scale, window_id, slot, lights_right_phys,
            ),
        );
    }

    /// Which window an AppKit callback is about.
    ///
    /// The context is the window: `dispatch_event_for` resolved it
    /// from the id the raising view or delegate carries, so a click in
    /// the second window cannot be attributed to the first.
    pub(super) fn event_window(ctx: &MarspotAppCtx) -> u32 {
        ctx.window_id()
    }

    /// Close ONE window: tell the core, drop our side, ask AppKit.
    ///
    /// Extracted from `close_requested` so the dev seam that a test
    /// uses runs this exact code rather than a second copy of it —
    /// the bug it guards against (over-released NSWindow) lives in
    /// what happens after `app::close_window`, so a paraphrase would
    /// not have caught it.
    /// Whether a window other than `window_id` is still open.  Asked of
    /// the shell's own list, which a close updates at once: the count
    /// AppKit keeps drops only when the NSWindow is torn down, after
    /// the dispatch, so two closes asked for together (a core that
    /// empties two windows in one pump) both saw "more than one" and
    /// the app stayed up with no window.
    pub(super) fn another_window_than(&self, window_id: u32) -> bool {
        self.windows.iter().any(|w| w.window_id != window_id)
    }

    pub(super) fn close_window_by_id(&mut self, window_id: u32, discard: bool) {
        // Tell the core first: it parks or discards the window while
        // the surfaces are still alive, so nothing renders into a
        // released pair on the way out.
        self.send(
            MsgType::WindowClosed,
            marspot::shell_proto::encode_window_closed(window_id),
        );
        self.pending_frame_index.remove(&window_id);
        if let Some(i) = self.window_index(window_id) {
            let w = self.windows.remove(i);
            if discard {
                self.forget_slot(w.frame_index);
            }
            if let Some(p) = w.surfaces {
                p.release();
            }
            if let Some(p) = w.pending_surfaces {
                p.release();
            }
        }
        // Queued: the NSWindow is torn down after this dispatch,
        // for the same re-entrancy reason opening one is.
        marspot::app::close_window(window_id);
        lx_event!(
            "WINDOW_CLOSED",
            "closed one window; app keeps running",
            window_id = window_id,
            shell_windows = self.windows.len()
        );
    }
}
