//! Handling one frame the active core sent.

use super::*;

impl ShellApp {
    /// Route a frame from the **active** core — it drives what's on
    /// screen.  `SurfaceReady` here is a resize handoff (same core, new
    /// size); the dual-core update path uses `handle_pending_msg`.
    pub(super) fn handle_active_msg(&mut self, msg: ShellInbox, ctx: &MarspotAppCtx) {
        match msg {
            ShellInbox::SurfaceReady(id) => self.on_surface_ready(id),
            ShellInbox::FramePaceAck => {
                if let Some(c) = self.active.as_mut() {
                    c.paced = true;
                }
            }
            ShellInbox::HelloAck(v) => {
                if v == PROTO_VERSION {
                    if let Some(c) = self.active.as_mut() {
                        c.hello_acked = true;
                        // The PONG deadline starts **here**, not at
                        // spawn.  `last_pong_at` is seeded at spawn as
                        // a freebie, but booting is not free: attach
                        // the surfaces, reattach thirteen L3s, build
                        // the layout.  On a loaded machine that took
                        // 20 s, so the moment this core became able to
                        // answer, its deadline had already expired and
                        // step 4 killed it in the same tick — 184 ms
                        // after the handshake it had just completed.
                        // The replacement hit the same wall, and the
                        // one after that, until the crash budget
                        // tripped and the window bricked behind
                        // "please restart the app"
                        // (2026-08-11T13:21–13:22Z, four restarts, one
                        // signature).  A deadline that starts before
                        // the peer can reply measures our boot, not
                        // its health.
                        let now = Instant::now();
                        c.last_pong_at = now;
                        c.next_ping_at = now + PING_INTERVAL;
                    }
                    lx_event!("HELLO_ACK", "core handshake OK", v = v);
                    sup_log::log("HELLO_ACK", &format!("v={v}"));
                    self.replay_pane_wheel_keys();
                    // Every core that becomes the active one comes
                    // through here (boot, restart, update swap).
                    self.pane_look.core_replaced();
                    self.flush_pane_decls();
                    self.show_early_frames();
                } else {
                    lx_event!(
                        "HELLO_MISMATCH",
                        "HelloAck disagrees with shell version; killing core",
                        core_v = v,
                        shell_v = PROTO_VERSION
                    );
                    sup_log::log("HELLO_MISMATCH", &format!("core_v={v} shell_v={PROTO_VERSION}"));
                    // Kill the child but keep the conn — `spawned_at` and
                    // `hello_acked == false` stay, so the HELLO timeout in
                    // poll_supervisor respawns it.
                    if let Some(c) = self.active.as_mut()
                        && let Some(mut child) = c.child.take() {
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                }
            }
            ShellInbox::Pong(nonce) => {
                if let Some(c) = self.active.as_mut()
                    && nonce == c.last_ping_nonce {
                        c.last_pong_at = Instant::now();
                    }
            }
            ShellInbox::CaretRect(rect, window_id) => {
                // NOT `ctx` — the control socket is drained in
                // `user_event`, which is an event about the process and
                // so always dispatched against the first window.  Using
                // `ctx` sent every window's caret to window 1's view;
                // the others answered `firstRectForCharacterRange:` with
                // a zero rect and the IME candidate window detached from
                // the caret.  The id on the wire names the right one.
                marspot::app::set_caret_rect_phys_for(window_id, rect);
            }
            // Legacy v=1 wake from the core's pre-A2-A4 single-
            // surface path: the core renders into ENV_SURFACE_ID
            // directly and pokes us with an empty FrameRendered.
            // We never get a SurfaceReady from such a core (it only
            // emits one on Resize), so we ALSO flip first_frame_ready
            // here — otherwise a fresh shell paired with a rolled-back
            // legacy core (`current/marspot-core` was reverted after
            // a probation-abort while the shell self-update succeeded)
            // would gate `redraw()` forever and paint black.
            // A v=2 core never sends this, so it's pure tolerance.
            ShellInbox::FrameRendered => {
                if let Some(wi) = self.window_index(ctx.window_id()) {
                    self.windows[wi].first_frame_ready = true;
                    self.windows[wi].frame_pending = true;
                }
            }
            ShellInbox::PaneBadgeClicked(shelld_sid) => {
                self.plugin_registry
                    .dispatch_pane_badge_click_with(&self.plugin_host, shelld_sid);
            }
            ShellInbox::PaneBadgeMenuRequest(shelld_sid, x, y) => {
                let items = self.plugin_registry.dispatch_pane_badge_menu(
                    &self.plugin_host,
                    shelld_sid,
                );
                // Every step of this path used to be silent, so a
                // right-click that did nothing left no trace of how far
                // it got — L2 not asking, L1 not receiving, no plugin
                // claiming the session, or a plugin returning nothing
                // all looked identical from the outside (2026-09-05).
                lx_debug!(
                    "shell.badge_menu.request",
                    "badge menu asked of the plugin registry",
                    shelld_session_id = shelld_sid,
                    items = items.len()
                );
                if items.is_empty() {
                    lx_warn!(
                        "shell.badge_menu.no_items",
                        "no plugin offered a badge menu; nothing will open",
                        shelld_session_id = shelld_sid
                    );
                }
                // Empty menu still gets no reply on purpose — L2
                // opens nothing either way, so the frame would be
                // dead weight.
                if !items.is_empty()
                    && let Some(conn) = self.active.as_ref() {
                        conn.send(
                            MsgType::PaneBadgeMenu,
                            marspot::shell_proto::encode_pane_badge_menu(
                                shelld_sid, x, y, &items,
                            ),
                        );
                    }
            }
            ShellInbox::PaneBadgeMenuAction(shelld_sid, tag) => {
                self.plugin_registry.dispatch_pane_badge_menu_action(
                    &self.plugin_host,
                    shelld_sid,
                    tag,
                );
            }
            ShellInbox::PaneSessionKey(sid, ev) => {
                self.dispatch_pane_session_key(sid, ev);
            }
            ShellInbox::PaneFocused(sid) => {
                // Straight to the pane's own session, if it has one:
                // a reclaimed pane starts restoring the moment the
                // user looks at it, instead of after they type.
                self.dispatch_pane_session_focus(sid);
                self.plugin_registry
                    .dispatch_pane_focused(&self.plugin_host, sid);
            }
            ShellInbox::PaneSessionUserEscape(sid) => {
                self.end_pane_session(sid, plugins::EndReason::UserEscape);
            }
            ShellInbox::WindowOpenRequest(frame_index, hint) => {
                if frame_index == marspot::shell_proto::WINDOW_OPEN_USER {
                    // A user action (move-pane-to-new-window), not a
                    // boot restore: none of the restore gates apply —
                    // safe mode blocks automatic multiplication, and
                    // an explicit user request is neither automatic
                    // nor a spawn (the pane already exists).
                    //
                    // RFC-006 §4 — a drag-to-desktop carries the
                    // release point (screen pts): the window opens
                    // centred there, so it is born where the pane was
                    // dropped.  AppKit's own constrain pass keeps it
                    // on-screen.
                    let frame = hint.map(|(cx, cy)| {
                        (
                            cx - DEFAULT_W_PT / 2.0,
                            cy - DEFAULT_H_PT / 2.0,
                            DEFAULT_W_PT,
                            DEFAULT_H_PT,
                        )
                    });
                    self.open_window_with_frame(frame, None);
                } else {
                    self.restore_window(frame_index);
                }
            }
            ShellInbox::IconMaskWanted(symbol, point_px) => match self.core_icons.want(symbol, point_px) {
                Some((s, p, side, mask)) => {
                    self.send(MsgType::IconMask, marspot::shell_proto::encode_icon_mask(s, p, side, &mask))
                }
                None => self.draw_core_icons(),
            },
            ShellInbox::WindowCloseRequest(window_id) => {
                // The core asked because this window emptied out — its
                // last pane was closed, or moved away.  Either way
                // there is nothing left in it.
                //
                // When it is the last window, an empty window means an
                // empty app: the user closed the final pane, and
                // marspot goes with it.  The core has already dropped
                // the saved layout; drop the saved geometry to match,
                // so the next launch opens a default window instead of
                // reusing the frame of what was just dismantled.
                if self.another_window_than(window_id) {
                    // The core only asks when the window emptied out:
                    // its panes are gone for good, so its slot goes too.
                    self.close_window_by_id(window_id, true);
                } else {
                    lx_event!(
                        "WINDOW_CLOSE_LAST_PANE",
                        "last pane of the last window closed — quitting",
                        window_id = window_id
                    );
                    if let Err(e) = marspot::state::clear_windows() {
                        lx_warn!("shell.window_state.clear_failed", &format!("{e}"));
                    }
                    self.saved_window_frames.clear();
                    self.quit(ctx);
                }
            }
        }
    }
}
