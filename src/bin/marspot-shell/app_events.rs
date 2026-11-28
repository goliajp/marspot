//! The AppKit callbacks: every window, input and lifecycle event the
//! app loop hands the shell.

use super::*;

impl MarspotApp for ShellApp {
    fn resumed(&mut self, ctx: &MarspotAppCtx) {
        let (w_phys, h_phys) = ctx.inner_size_phys();
        let scale = ctx.scale();
        let w_px = w_phys.max(64.0) as usize;
        let h_px = h_phys.max(64.0) as usize;

        // RFC-001 plugin bootstrap: register first-party plugins,
        // run init then start.  Plugins are passive until the core
        // has booted + sessions attached, so start is fine right
        // here — plugin tick is what drives per-pane work later.
        if self.plugin_registry.is_empty() {
            self.plugin_registry
                .register(Box::new(plugins::claudecode::ClaudecodePlugin::new()));
            // RFC-008 — the same pane affordances for the OpenAI CLI and
            // for Google's.  A pane runs one agent at a time, and each
            // plugin claims only the sessions whose process tree it
            // recognises, so all can be registered unconditionally.
            self.plugin_registry.register(Box::new(plugins::codex::CodexPlugin::new()));
            self.plugin_registry.register(Box::new(plugins::gemini::GeminiPlugin::new()));
            self.plugin_registry.register(Box::new(plugins::kimi::KimiPlugin::new()));
            // Codex panes as messaging peers of Claude Code sessions.
            self.plugin_registry.register(Box::new(plugins::peerbus::PeerbusPlugin::new()));
            self.plugin_registry.register(Box::new(plugins::helpers::HelpersPlugin::new()));
            // The account usage feed, when no external collector writes one.
            self.plugin_registry.register(Box::new(plugins::usage::UsagePlugin::new()));
            self.plugin_registry.register(Box::new(plugins::zombie::ZombiePlugin::new()));
            self.plugin_registry.init_all_with(&self.plugin_host);
            self.plugin_registry.start_all_with(&self.plugin_host);
        }

        let pair = match SurfacePair::create(w_px, h_px) {
            Ok(p) => p,
            Err(e) => {
                lx_error!(
                    "shell.resumed.pair_create_failed",
                    &e.to_string(),
                    w = w_px,
                    h = h_px
                );
                ctx.exit();
                return;
            }
        };

        let (front_id, back_id) = pair.ids();
        // `resumed` fires for the boot window, which the app state
        // created before any callback could run.
        let wi = self.window_index(ctx.window_id()).unwrap_or(0);
        self.windows[wi].surfaces = Some(pair);
        self.windows[wi].layers = Some(FrameLayers::new(ctx.ns_view(), ctx.window_id(), scale));

        self.active = self.spawn_core(front_id, back_id, w_px, h_px, scale);
        if self.active.is_some() {
            self.record_core_boot();
        }
        self.start_redraw_pump();
        ctx.request_redraw();
        // Dev-only seam: open N extra windows exactly as Cmd-N does.
        // A keystroke cannot be delivered to the sandbox app from a
        // script, and the fresh-window path (1×1 grid, one brand-new
        // session) is otherwise untestable — the restore path covers
        // everything except that.  Unset in the installed app.
        if std::env::var("MARSPOT_DEV_CLOSE_EXTRA").is_ok() {
            self.dev_close_extra = true;
        }
        if let Ok(n) = std::env::var("MARSPOT_DEV_EXTRA_WINDOWS") {
            let n: usize = n.parse().unwrap_or(0);
            for _ in 0..n.min(8) {
                self.open_new_window();
            }
            if n > 0 {
                lx_event!(
                    "DEV_EXTRA_WINDOWS",
                    "opened extra windows on request (dev seam)",
                    n = n
                );
            }
        }
    }

    fn user_event(&mut self, ctx: &MarspotAppCtx) {
        // Drain both cores' inboxes — each reader thread calls
        // `proxy.wake()` after a push, so at least one message is ready.
        // Active and pending get separate routing: the active core
        // drives what's on screen (resize swap, caret, healthcheck);
        // the pending core only reports its probation progress.
        let mut active_inbox = self.take_inbox();
        let owed = self.frame_owed();
        let knocked = self.knocked(&active_inbox);
        for msg in active_inbox.drain(..) {
            self.handle_active_msg(msg, ctx);
        }
        self.keep_inbox(active_inbox);
        self.poll_supervisor(ctx);
        self.settle_pump(knocked, owed);
        self.dev_pass();
        ctx.request_redraw();
    }

    fn key_event(&mut self, ctx: &MarspotAppCtx, event: MarspotKeyEvent, mods: Modifiers) {
        // Forensic anchor for "key X did the wrong thing" reports.
        // We log the LogicalKey and the modifier byte; the actual PTY
        // bytes are encoded downstream in L3 (input_core::key_event_to_bytes).
        // This line being at DEBUG means production
        // (default INFO) pays nothing; `MARSPOT_LOG_SHELL=debug` opens
        // the floodgate when investigating.  Mod byte format mirrors
        // shell_proto::struct_to_mods_byte: shift=1 ctrl=2 alt=4 super=8.
        lx_debug!(
            "input.key",
            "key event forwarded to core",
            logical = format!("{:?}", event.logical),
            mods_byte = struct_to_mods_byte(mods),
            state = format!("{:?}", event.state)
        );
        let window = Self::event_window(ctx);
        let sent = key_send::forward_key(
            self.pump.as_ref(),
            self.active.as_ref(),
            &mut self.key_buf,
            &event,
            mods,
            window,
        );
        if !sent {
            self.open_new_window();
        }
    }

    fn mouse_down(&mut self, ctx: &MarspotAppCtx, x: f64, y: f64, mods: Modifiers) {
        let w = Self::event_window(ctx);
        self.send(MsgType::MouseDown, encode_mouse(x, y, struct_to_mods_byte(mods), w));
    }

    fn mouse_right_down(&mut self, ctx: &MarspotAppCtx, x: f64, y: f64, mods: Modifiers) {
        let w = Self::event_window(ctx);
        self.send(MsgType::MouseRightDown, encode_mouse(x, y, struct_to_mods_byte(mods), w));
    }

    fn mouse_drag(
        &mut self,
        ctx: &MarspotAppCtx,
        x: f64,
        y: f64,
        hover_window_id: u32,
        hover_x: f64,
        hover_y: f64,
    ) {
        // No modifier info on drag — pass zero; the renderer doesn't
        // currently need mods for drag-extend selection.
        let w = Self::event_window(ctx);
        self.send(
            MsgType::MouseDrag,
            marspot::shell_proto::encode_mouse_drag(
                x, y, 0, w, hover_window_id, hover_x, hover_y,
            ),
        );
    }

    fn mouse_up(
        &mut self,
        ctx: &MarspotAppCtx,
        x: f64,
        y: f64,
        drop_window_id: u32,
        drop_x: f64,
        drop_y: f64,
    ) {
        let w = Self::event_window(ctx);
        self.send(
            MsgType::MouseUp,
            marspot::shell_proto::encode_mouse_up(
                x, y, 0, w, drop_window_id, drop_x, drop_y,
            ),
        );
    }

    fn file_drop(&mut self, ctx: &MarspotAppCtx, x: f64, y: f64, paths: &[String]) {
        // L2 owns the pane layout — forward drop point + raw paths;
        // it hit-tests the pane and shell-quotes before insertion.
        let w = Self::event_window(ctx);
        self.send(MsgType::FileDrop, encode_file_drop(x, y, paths, w));
    }

    fn mouse_moved(&mut self, ctx: &MarspotAppCtx, x: f64, y: f64) {
        // Forwarded raw — L2 hit-tests against chrome rects and
        // ignores moves that don't change its hover region (cheap).
        let w = Self::event_window(ctx);
        self.send(MsgType::MouseMove, encode_mouse(x, y, 0, w));
    }

    fn scroll(&mut self, ctx: &MarspotAppCtx, dx: f64, dy: f64, precise: bool) {
        let w = Self::event_window(ctx);
        self.send(MsgType::Scroll, encode_scroll(dx, dy, precise, w));
    }

    /// The OS's own controls moved (full-screen transition) while the
    /// window's pixels did not.  One small frame, no surface churn —
    /// see `MsgType::WindowChrome`.
    fn chrome_changed(&mut self, ctx: &MarspotAppCtx) {
        self.send(
            MsgType::WindowChrome,
            encode_window_chrome(
                Self::event_window(ctx),
                ctx.traffic_lights_right_phys(),
            ),
        );
    }

    fn resized(&mut self, ctx: &MarspotAppCtx, w_phys: f64, h_phys: f64) {
        // F3+6.1 — persist window frame on every resize step.  Atomic
        // rename means a live drag can fire 60+ saves/s and the file
        // is always valid; the cost (~50us memcpy + 1 syscall) is
        // well below the per-frame resize budget.
        self.save_window_state_if_changed(ctx);
        let Some(wi) = self.window_index(ctx.window_id()) else { return };
        let scale = ctx.scale();
        let w_px = w_phys.max(64.0) as usize;
        let h_px = h_phys.max(64.0) as usize;
        let geom = AttachedGeometry {
            w_px,
            h_px,
            scale,
            lights_right_phys: ctx.traffic_lights_right_phys(),
        };
        // What the notification is worth is decided before any of it is
        // acted on.  AppKit sends these in streams — 2026-09-27, ~144 a
        // second for a window nobody was resizing — and the old code
        // did the whole resize for each: a present, a fresh IOSurface
        // pair, two more Metal textures in the core, and a presenter
        // pointed at a surface the core had only just started painting.
        // That last one is a window flickering between two states, and
        // it was reported as exactly that.
        let work = resize_work(self.windows[wi].last_attached, &geom);
        if work != ResizeWork::Attach {
            self.windows[wi].skipped_resizes += 1;
            if self.windows[wi].skipped_resizes == 1 {
                lx_info!(
                    "shell.resize.no_new_geometry",
                    "resize notification with nothing new in it — no surface churn",
                    window_id = self.windows[wi].window_id,
                    w_px = w_px as u64,
                    h_px = h_px as u64
                );
            }
            if work == ResizeWork::ChromeOnly {
                // The buttons moved without the pixels (a full-screen
                // transition): one small frame, and a redraw to show it.
                self.windows[wi].last_attached = Some(geom);
                self.chrome_changed(ctx);
                ctx.request_redraw();
            }
            return;
        }
        // Read the gate before taking `&mut` on the same window.
        let ready = self.windows[wi].first_frame_ready;
        if let Some(l) = self.windows[wi].layers.as_mut() {
            l.resized(w_phys, h_phys, scale);
            // A copying presenter draws *synchronously* inside the
            // resize callback, so its drawable lands in the same
            // CATransaction AppKit is about to commit for the new
            // bounds: the window edge and the contents move together.
            if ready && let Some(p) = l.copying() {
                p.present();
            }
        }
        // Fire the IOSurface handoff *immediately* (no debounce).
        // With presents-with-transaction the swap between old and new
        // IOSurface lands in the same CATransaction as the window
        // resize, so the visual stays stable while the terminal grid
        // actually reflows.  Without this, the pane grid never gets
        // SIGWINCH during a drag and lines wrap at the old column
        // count — what the user observed as "换行没跟上".
        let skipped = std::mem::take(&mut self.windows[wi].skipped_resizes);
        match self.fresh_pending_pair(wi, w_px, h_px) {
            Ok((f, b)) => {
                self.windows[wi].last_attached = Some(geom);
                lx_info!(
                    "shell.resize.attached",
                    "new pair for a window whose pixels moved",
                    window_id = self.windows[wi].window_id,
                    w_px = w_px as u64,
                    h_px = h_px as u64,
                    skipped_since_last = skipped as u64
                );
                self.send_surface_attach(
                    Self::event_window(ctx), f, b, w_phys, h_phys, scale,
                    ctx.traffic_lights_right_phys(),
                );
            }
            Err(e) => {
                lx_error!("shell.resize.pair_create_failed", &e.to_string());
            }
        }
        ctx.request_redraw();
    }

    fn moved(&mut self, ctx: &MarspotAppCtx) {
        // F3+6.1 — drag-end has no explicit callback in AppKit's
        // delegate vocabulary; `windowDidMove:` fires per-step during
        // the drag instead.  Each step writes the bin — atomic rename
        // means the file is always consistent and the cost is cheap.
        self.save_window_state_if_changed(ctx);
    }

    fn focused(&mut self, ctx: &MarspotAppCtx, focused: bool) {
        self.send(MsgType::Focus, encode_focus(focused));
        if focused {
            // keys and the core's raise follow the key window
            self.send(MsgType::WindowFocus, marspot::shell_proto::encode_window_focus(Self::event_window(ctx)));
        }
        self.proxy.wake(); // a pass now: what is queued below is driven by the clock
        let returning = focused && !self.app_focused;
        self.app_focused = focused;
        if returning {
            self.queue_parked_wakes();
        }
        // Silent-update trigger: the user just left marspot's window
        // (cmd-tab, click on another app, minimise).  If a pending
        // binary is staged in `binaries/pending/`, this is the
        // cheapest moment to swap — they're not watching us repaint.
        // The shell window stays put through the swap, the new core
        // attaches to the same IOSurface and shelld session, so when
        // they come back they see the same content rendered by the
        // new version's renderer.
        //
        // Skipped while a probe is in flight: one staged binary at a
        // time, and the probe already decided what happens next.
        if !focused && matches!(self.sup_state, SupervisorState::Idle)
            && std::env::var_os("MARSPOT_MANUAL_UPDATE_ONLY").is_none() {
                self.apply_pending_update();
            }
    }

    fn minimized(&mut self, ctx: &MarspotAppCtx, minimized: bool) {
        let id = Self::event_window(ctx);
        if let Some(i) = self.window_index(id) {
            self.windows[i].minimized = minimized;
        }
        self.send(MsgType::WindowMinimized, marspot::shell_proto::encode_window_minimized(id, minimized));
        lx_event!("WINDOW_MINIMIZED", "a window was minimized or restored", window_id = id, minimized = minimized as u32);
    }

    fn ime_preedit_changed(&mut self, ctx: &MarspotAppCtx, text: &str) {
        let w = Self::event_window(ctx);
        self.send(MsgType::Preedit, encode_preedit(text, w));
    }

    fn window_opened(&mut self, ctx: &MarspotAppCtx) {
        // The window exists now, so its own metrics are readable — a
        // window opened on another display can have a different
        // backing scale, and sizing its pair off the key window would
        // hand it a mismatched surface.
        let window_id = ctx.window_id();
        let (w_phys, h_phys) = ctx.inner_size_phys();
        let scale = ctx.scale();
        let pair = match SurfacePair::create(
            w_phys.max(64.0) as usize,
            h_phys.max(64.0) as usize,
        ) {
            Ok(p) => p,
            Err(e) => {
                lx_error!("shell.window.pair_create_failed", &e.to_string());
                marspot::app::close_window(window_id);
                return;
            }
        };
        let (f, b) = pair.ids();
        let slot = self
            .pending_frame_index
            .remove(&window_id)
            .unwrap_or_else(|| self.next_frame_index());
        let mut win = ShellWindow::new(window_id, slot);
        // A window without this shows nothing, however faithfully the
        // core paints (2026-07-28: Cmd-N opened a black rectangle).
        win.layers = Some(FrameLayers::new(ctx.ns_view(), window_id, scale));
        lx_event!(
            "WINDOW_PRESENTER_READY",
            "layers attached to the new window's view",
            window_id = window_id
        );
        win.pending_surfaces = Some(pair);
        // Seed the geometry cache from the window as it actually
        // opened.  `window-state.bin` is a list written whole on every
        // change, so a window with no cached frame would leave a hole
        // in it and shift every later window's entry.
        let (fx, fy, fw, fh) = ctx.window_frame_pt();
        win.last_saved_window = Some((
            fx.round(),
            fy.round(),
            fw.round(),
            fh.round(),
            ctx.window_display_id().unwrap_or(0),
        ));
        if self.dev_close_extra && window_id != marspot::shell_proto::FIRST_WINDOW_ID {
            win.dev_close_after_paint = true;
        }
        self.windows.push(win);
        self.save_window_frames();
        // A `SurfaceAttachWindow` naming a window the core has not seen
        // IS that window's birth event — there is no separate create
        // frame.
        self.send_surface_attach(
            window_id, f, b, w_phys, h_phys, scale,
            ctx.traffic_lights_right_phys(),
        );
        lx_event!(
            "WINDOW_OPENED",
            "new window announced to core",
            window_id = window_id,
            windows = self.windows.len()
        );
    }

    fn quit_requested(&mut self, ctx: &MarspotAppCtx) {
        // Quit means the app.  It arrives with whichever window is
        // still open, and reading it as "close that window" is how
        // quitting from the dock used to leave every other window on
        // screen (2026-09-28 report).
        self.quit(ctx);
    }

    fn close_requested(&mut self, ctx: &MarspotAppCtx) {
        // RFC-005 — closing one of several windows closes THAT window
        // and the panes it holds; only the last window closing is a
        // quit.  Cmd-Q is `quit_requested`, not this.
        let window_id = Self::event_window(ctx);
        if self.another_window_than(window_id) {
            // The red button / Cmd-W: putting a window away, not
            // dismantling it.  Its slot is kept so the layout and
            // the geometry both come back on the next launch.
            self.close_window_by_id(window_id, false);
            return;
        }
        self.quit(ctx);
    }

    fn redraw(&mut self, _ctx: &MarspotAppCtx) {
        self.present_windows();
    }
}
