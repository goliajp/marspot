//! What each frame from L2 becomes for the session's main loop.

use std::sync::mpsc::Sender;

use marspot_term::shell_proto::{
    MsgType, decode_frame_cells, decode_get_selection_text, decode_grid_resize, decode_grid_scroll,
    decode_key_event, decode_pane_type, decode_pane_watch, decode_paste, decode_prompt_jump, decode_search_cancel,
    decode_search_scrollback, wire_to_event,
};

use crate::SessionEvent;
use crate::pane_ctl::Request;

/// Hand one frame to the main loop; false once nobody is listening.
/// `payload` is lent for this call: an event that keeps it decodes or
/// copies it.
pub(super) fn dispatch(
    t: MsgType,
    payload: &[u8],
    tx: &Sender<SessionEvent>,
    generation: u64,
) -> bool {
    match t {
        MsgType::KeyEvent => {
            // The trailing window id is an L1↔L2 concern; a
            // session belongs to a pane, and a pane does not
            // care which window draws it.
            if let Ok((w, _window_id)) = decode_key_event(payload) {
                let (e, m) = wire_to_event(w);
                if tx.send(SessionEvent::Key(e, m)).is_err() {
                    return false;
                }
                crate::input::arrived();
            }
        }
        MsgType::GridResize => {
            if let Ok((cols, rows)) = decode_grid_resize(payload)
                && tx.send(SessionEvent::Resize(cols, rows)).is_err()
            {
                return false;
            }
        }
        MsgType::GridScroll => {
            if let Ok(off) = decode_grid_scroll(payload)
                && tx.send(SessionEvent::Scroll(off)).is_err()
            {
                return false;
            }
        }
        MsgType::PromptJump => {
            if let Ok(dir) = decode_prompt_jump(payload)
                && tx.send(SessionEvent::PromptJump(dir)).is_err()
            {
                return false;
            }
        }
        MsgType::GetSelectionText => {
            if let Ok((seq, a, fo, bw)) = decode_get_selection_text(payload)
                && tx.send(SessionEvent::GetSelection(seq, a, fo, bw)).is_err()
            {
                return false;
            }
        }
        MsgType::Paste => {
            if let Ok(text) = decode_paste(payload) {
                if tx.send(SessionEvent::Paste(text)).is_err() {
                    return false;
                }
                crate::input::arrived();
            }
        }
        MsgType::PaneHoldGrid => {
            if let Ok((_sid, on)) = marspot_term::shell_proto::decode_pane_hold_grid(payload)
                && tx.send(SessionEvent::HoldGrid(on)).is_err()
            {
                return false;
            }
        }
        MsgType::PaneResetAttrs => {
            if marspot_term::shell_proto::decode_pane_reset_attrs(payload).is_ok()
                && tx.send(SessionEvent::ResetAttrs).is_err()
            {
                return false;
            }
        }
        MsgType::PaneRenderMarkup => {
            if let Ok((_sid, on)) = marspot_term::shell_proto::decode_pane_render_markup(payload)
                && tx.send(SessionEvent::RenderMarkup(on)).is_err()
            {
                return false;
            }
        }
        MsgType::InjectInput => {
            if let Ok((_sid, bytes)) = marspot_term::shell_proto::decode_inject_input(payload) {
                if tx.send(SessionEvent::InjectInput(bytes)).is_err() {
                    return false;
                }
                crate::input::arrived();
            }
        }
        MsgType::PanePointerInput => {
            if let Ok((_sid, ev)) = marspot_term::shell_proto::decode_pane_pointer_input(payload)
                && tx.send(SessionEvent::Pointer(ev)).is_err()
            {
                return false;
            }
        }
        MsgType::PaneTerminalFocus => {
            if let Ok((_sid, focused)) =
                marspot_term::shell_proto::decode_pane_terminal_focus(payload)
                && tx.send(SessionEvent::TerminalFocus(focused)).is_err()
            {
                return false;
            }
        }
        MsgType::PaneResetMouseReporting => {
            if marspot_term::shell_proto::decode_pane_reset_mouse_reporting(payload).is_ok()
                && tx.send(SessionEvent::ResetMouseReporting).is_err()
            {
                return false;
            }
        }
        MsgType::SearchScrollback => {
            if let Ok((query_id, case_sensitive, max_total, query)) =
                decode_search_scrollback(payload)
                && tx
                    .send(SessionEvent::SearchRequest {
                        query_id,
                        case_sensitive,
                        max_total,
                        query,
                    })
                    .is_err()
            {
                return false;
            }
        }
        MsgType::SearchCancel => {
            if let Ok(query_id) = decode_search_cancel(payload)
                && tx
                    .send(SessionEvent::SearchCancelRequested(query_id))
                    .is_err()
            {
                return false;
            }
        }
        MsgType::SessionUpdate if tx.send(SessionEvent::UpdateRequested).is_err() => return false,
        MsgType::SessionFrameCells => {
            let version = decode_frame_cells(payload).unwrap_or(0);
            if tx
                .send(SessionEvent::FrameCells(generation, version))
                .is_err()
            {
                return false;
            }
        }
        MsgType::PaneType => {
            if let Ok(t) = decode_pane_type(payload) {
                if tx
                    .send(SessionEvent::PaneControl(Request::Type(t)))
                    .is_err()
                {
                    return false;
                }
                crate::input::arrived();
            }
        }
        MsgType::PaneWatch => {
            if let Ok(w) = decode_pane_watch(payload)
                && tx
                    .send(SessionEvent::PaneControl(Request::Watch(w)))
                    .is_err()
            {
                return false;
            }
        }
        MsgType::SessionShadow => {
            if let Ok(m) = marspot_term::shell_proto::decode_session_shadow(payload)
                && tx.send(SessionEvent::Shadow(m)).is_err()
            {
                return false;
            }
        }
        MsgType::SessionRepublish => {
            if let Ok(name) = String::from_utf8(payload.to_vec())
                && tx.send(SessionEvent::Republish(name)).is_err()
            {
                return false;
            }
        }
        _ => {}
    }
    true
}
