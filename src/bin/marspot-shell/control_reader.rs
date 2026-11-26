//! The thread that reads what the core sends and hands it to the main
//! thread.

use std::os::unix::net::UnixStream;
use std::sync::mpsc::Sender;

use marspot::app::EventProxy;
use marspot::latency_trace::{self as trace, Stage};
use marspot::lx_error;
use marspot::shell_proto::{
    decode_caret_rect, decode_hello_ack, decode_pong, decode_surface_ready, Frame, MsgType,
};

/// Frames the reader thread parses off the control socket and hands
/// to the main thread.
pub(crate) enum ShellInbox {
    SurfaceReady(u32),
    FramePaceAck,
    HelloAck(u32),
    Pong(u32),
    /// Focused-pane caret rect from the core (view-local physical
    /// pixels), forwarded to AppKit so the IME candidate window
    /// anchors under the caret.
    /// Caret rect plus the window it belongs to — the shell parks the
    /// IME candidate panel against that window.
    CaretRect(Option<(f64, f64, f64, f64)>, u32),
    /// Whether a window's keys are a helper strip's.
    StripKeys(marspot::shell_proto::StripKeys),
    /// Core rendered a fresh frame into the IOSurface — present it.  Carries
    /// nothing; its arrival (via the reader's `proxy.wake()`) drives the
    /// per-frame present, replacing the blind ~60 fps redraw timer.
    FrameRendered,
    /// L2 → L1: user clicked the active prefix of a pane badge on the
    /// pane backing this shelld session.  Dispatched to every loaded
    /// plugin so whichever set the badge can react (claudecode → cycle
    /// the next profile and rerun `claudeN --resume <uuid>`).
    PaneBadgeClicked(u64),
    /// core → shell: the user's focus moved to this pane.
    PaneFocused(u64),
    /// L2 → L1: user right-clicked the badge prefix — collect menu
    /// rows from the plugins and reply with a `PaneBadgeMenu` frame.
    /// `(sid, anchor_x, anchor_y)`; the anchor is echoed back so the
    /// exchange is stateless.
    PaneBadgeMenuRequest(u64, f64, f64),
    /// L2 → L1: user picked a badge-menu row.  `(sid, tag)` where
    /// `tag` is the plugin-assigned id from the menu frame.
    PaneBadgeMenuAction(u64, u32),
    /// L2 → L1: a keystroke arrived on a pane held by a LOCK_KEYS
    /// PaneSession.  Routed to the matching session's on_user_key.
    PaneSessionKey(u64, marspot::shell_proto::WireKeyEvent),
    /// L2 → L1: user pressed Esc 3× in 5 s; force-end the session
    /// regardless of plugin opinion.
    PaneSessionUserEscape(u64),
    /// L2 → L1: reopen a window the last session had, restoring its
    /// geometry from this entry of `window-state.bin`.  RFC-005 step
    /// 6b — the core knows how many windows the user had, L1 owns the
    /// windows themselves, so the core asks.
    WindowOpenRequest(u32, Option<(f64, f64)>),
    /// L2 → L1: close this window (RFC-005 step 5 — its last pane was
    /// just moved out).  Runs the exact red-button path.
    WindowCloseRequest(u32),
    /// The core wants a toolbar icon drawn: `(symbol, point_px)`.
    IconMaskWanted(u16, u16),
    /// The keyboard confirmed a `/model` in this pane, and it was sent.
    ModelConfirmed(u64),
}

pub(crate) mod pane_wakes;

pub(crate) fn control_reader_loop(mut stream: UnixStream, tx: Sender<ShellInbox>, proxy: EventProxy) {
    loop {
        match Frame::read_from(&mut stream) {
            // the core is gone: a pass finds that out now
            Ok(None) => return proxy.wake(),
            Ok(Some(frame)) => {
                // answers to the control socket go straight to who waits
                if crate::pane_control::hub().is_some_and(|h| h.take_core_frame(frame.msg_type, &frame.payload)) {
                    continue;
                }
                let msg = match frame.msg_type {
                    MsgType::SurfaceReady => {
                        trace::mark(Stage::ShellSurfaceRead, 0);
                        decode_surface_ready(&frame.payload).ok().map(ShellInbox::SurfaceReady)
                    }
                    MsgType::HelloAck => {
                        decode_hello_ack(&frame.payload).ok().map(ShellInbox::HelloAck)
                    }
                    MsgType::FramePaceAck => Some(ShellInbox::FramePaceAck),
                    MsgType::Pong => decode_pong(&frame.payload).ok().map(ShellInbox::Pong),
                    MsgType::CaretRect => decode_caret_rect(&frame.payload)
                        .ok()
                        .map(|(rect, win)| ShellInbox::CaretRect(rect, win)),
                    // Empty-payload wake: a fresh frame is in the IOSurface.
                    // Mapping it to Some(..) is what makes `proxy.wake()`
                    // fire below → user_event → present.
                    MsgType::FrameRendered => Some(ShellInbox::FrameRendered),
                    MsgType::PaneBadgeClicked => {
                        marspot::shell_proto::decode_pane_badge_clicked(&frame.payload)
                            .ok()
                            .map(ShellInbox::PaneBadgeClicked)
                    }
                    MsgType::StripKeys => marspot::shell_proto::decode_strip_keys(&frame.payload).ok().map(ShellInbox::StripKeys),
                    MsgType::PaneFocused => {
                        marspot::shell_proto::decode_pane_focused(&frame.payload)
                            .ok()
                            .map(ShellInbox::PaneFocused)
                    }
                    MsgType::PaneBadgeMenuRequest => {
                        marspot::shell_proto::decode_pane_badge_menu_request(&frame.payload)
                            .ok()
                            .map(|(sid, x, y)| ShellInbox::PaneBadgeMenuRequest(sid, x, y))
                    }
                    MsgType::PaneBadgeMenuAction => {
                        marspot::shell_proto::decode_pane_badge_menu_action(&frame.payload)
                            .ok()
                            .map(|(sid, tag)| ShellInbox::PaneBadgeMenuAction(sid, tag))
                    }
                    MsgType::PaneSessionKey => {
                        marspot::shell_proto::decode_pane_session_key(&frame.payload)
                            .ok()
                            .map(|(sid, ev)| ShellInbox::PaneSessionKey(sid, ev))
                    }
                    MsgType::PaneSessionUserEscape => {
                        marspot::shell_proto::decode_pane_session_user_escape(&frame.payload)
                            .ok()
                            .map(ShellInbox::PaneSessionUserEscape)
                    }
                    MsgType::WindowOpenRequest => {
                        marspot::shell_proto::decode_window_open_request_at(&frame.payload)
                            .ok()
                            .map(|(idx, hint)| ShellInbox::WindowOpenRequest(idx, hint))
                    }
                    MsgType::WindowCloseRequest => {
                        marspot::shell_proto::decode_window_close_request(&frame.payload)
                            .ok()
                            .map(ShellInbox::WindowCloseRequest)
                    }
                    MsgType::IconMaskWanted => marspot::shell_proto::decode_icon_mask_wanted(&frame.payload)
                        .ok()
                        .map(|(symbol, point_px)| ShellInbox::IconMaskWanted(symbol, point_px)),
                    MsgType::ModelConfirmed => {
                        marspot::shell_proto::decode_model_confirmed(&frame.payload).ok().map(ShellInbox::ModelConfirmed)
                    }
                    // kept where the supervisor reads it, not queued:
                    // a pane that prints reports every second
                    MsgType::PaneAwake => {
                        if let Ok(a) = marspot::shell_proto::decode_pane_awake(&frame.payload)
                            && pane_wakes::record(&a)
                        {
                            crate::pump::knock();
                        }
                        None
                    }
                    // Unknown frames are ignored — keeps forward
                    // compatibility while the protocol grows.
                    _ => None,
                };
                if let Some(m) = msg {
                    if tx.send(m).is_err() {
                        return;
                    }
                    proxy.wake();
                }
            }
            Err(e) => {
                lx_error!("shell.control_reader.error", &format!("{e}"));
                return proxy.wake();
            }
        }
    }
}
