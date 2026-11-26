//! The core's end of one pane's L2↔L3 control socket.

use super::{decode_selection_text, unwatched, CoreEvent};
use marspot::latency_trace::{self as trace, Stage};
use marspot::shell_proto::{Frame, MsgType};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

/// Background reader on the L2↔L3 control socket: an L3 session pokes
/// `GridReady` whenever it republishes its grid; we turn each into an
/// `L3Ready` wake so the main loop re-reads the shm mirror promptly.
/// EOF / error just ends the thread — the pane's own `try_wait` detects
/// the child's exit.  A shadow pane (`unwatched`) wakes nobody for a
/// frame, and its bells and clipboard writes go nowhere: nobody is
/// looking, and a program nobody watches must not change the clipboard.
pub(super) fn l3_reader_loop(
    mut stream: UnixStream,
    session_id: u64,
    tx: marspot_term::mailbox::Sender<CoreEvent>,
    selection_tx: Sender<(u32, String)>,
) {
    // A bell goes to the loop, which knows the pane's window.  While one
    // is still queued a new one is folded into it: the queued bell is
    // judged no earlier than the new one arrived, so the new one would
    // fall inside the same window's quiet interval anyway.
    let bell_queued = Arc::new(AtomicBool::new(false));
    loop {
        match Frame::read_from(&mut stream) {
            Ok(Some(f)) => match f.msg_type {
                MsgType::GridReady => {
                    trace::mark(Stage::GridReady, session_id as u32);
                    if !unwatched::is(session_id) && tx.send(CoreEvent::L3Ready).is_err() {
                        return;
                    }
                }
                MsgType::PaneBell if unwatched::is(session_id) => {}
                MsgType::PaneBell => {
                    if !bell_queued.swap(true, Ordering::AcqRel)
                        && tx.send(CoreEvent::PaneBell(session_id, bell_queued.clone())).is_err()
                    {
                        return;
                    }
                }
                // OSC 52: L3 has no AppKit, so a clipboard write lands here and
                // is written from the reader thread (the pasteboard has its own
                // lock; the event loop would make a yank wait on a redraw)
                MsgType::PaneClipboardWrite if unwatched::is(session_id) => {
                    marspot::lx_event!("SHADOW_CLIPBOARD_DROPPED", "a shadow pane's clipboard write was dropped",
                        session_id = session_id, len = f.payload.len());
                }
                MsgType::PaneClipboardWrite => {
                    if let Ok(text) = std::str::from_utf8(&f.payload) {
                        marspot::input::write_clipboard_text(text);
                    }
                }
                MsgType::SessionPublishes
                | MsgType::SessionReadsFrameCells
                | MsgType::SessionReadsPaneControl
                | MsgType::PaneAwake => {
                    let _ = tx.send(CoreEvent::L3Says(session_id, f));
                }
                MsgType::PaneTyped | MsgType::PaneWatchFired | MsgType::PaneDone => {
                    let mut f = f;
                    // a session speaks only for itself, whatever sid it wrote
                    let at = if f.msg_type == MsgType::PaneTyped { 4 } else { 0 };
                    if f.msg_type != MsgType::PaneDone
                        && let Some(b) = f.payload.get_mut(at..at + 8)
                    {
                        b.copy_from_slice(&session_id.to_le_bytes());
                    }
                    let _ = tx.send(CoreEvent::PaneReport(session_id, f));
                }
                // Reply to a Cmd-C `GetSelectionText`, for whoever waits in
                // `L3Conn::request_selection_text`; its seq lets the waiter
                // discard a late reply to a request that timed out.
                MsgType::SelectionText => {
                    if let Ok(reply) = decode_selection_text(&f.payload) {
                        let _ = selection_tx.send(reply);
                    }
                }
                // a stale qid is dropped by the pane's `apply_results`
                MsgType::SearchResults => {
                    if let Ok((qid, has_more, total_seen, hits)) =
                        marspot::shell_proto::decode_search_results(&f.payload)
                    {
                        let _ = tx.send(CoreEvent::SearchResults(
                            session_id,
                            qid,
                            has_more,
                            total_seen,
                            hits,
                        ));
                    }
                }
                _ => {}
            },
            Ok(None) | Err(_) => {
                // Tell main loop the pane's L2↔L3 stream went away —
                // it'll wait_and_connect, swap the pane's control,
                // and respawn this reader on the new stream.
                let _ = tx.send(CoreEvent::L3ControlEof(session_id));
                return;
            }
        }
    }
}
