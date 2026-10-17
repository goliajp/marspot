//! The threads that read L2's frames off a control stream.
//!
//! One per client stream: the stream L3 was started with, one adopted
//! across an execv, one accepted on the session socket.  Each turns
//! frames into `SessionEvent`s for the main loop.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use marspot_term::shell_proto::{
    decode_get_selection_text, decode_grid_resize, decode_grid_scroll, decode_key_event,
    decode_paste, decode_search_cancel, decode_search_scrollback, wire_to_event, Frame, MsgType,
};

use crate::SessionEvent;

/// Every control reader of this image, and the means to stop them.
pub(crate) struct ControlReaders {
    stop_tx: UnixStream,
    stop_rx: Arc<UnixStream>,
    threads: Vec<JoinHandle<()>>,
}

impl ControlReaders {
    pub(crate) fn new() -> std::io::Result<Self> {
        let (stop_tx, stop_rx) = UnixStream::pair()?;
        Ok(Self {
            stop_tx,
            stop_rx: Arc::new(stop_rx),
            threads: Vec::new(),
        })
    }

    /// Read `stream` until it ends.  Shared by the inherited-fd path,
    /// the execv resume and the UDS accept path, so all of them speak
    /// the same protocol and unknown-frame handling lives in one place.
    /// `generation` tags the CoreGone sent on EOF, so a stale reader
    /// (from a swapped-out L2 socket) cannot null the current poke.
    pub(crate) fn spawn(&mut self, stream: UnixStream, tx: Sender<SessionEvent>, generation: u64) {
        self.threads.retain(|t| !t.is_finished());
        let stop = Arc::clone(&self.stop_rx);
        self.threads
            .push(std::thread::spawn(move || read_frames(stream, &stop, tx, generation)));
    }

    /// Stop every reader between two frames and wait for it.
    ///
    /// For an execv: a reader that had taken a frame off its socket when
    /// the image was replaced lost it -- a key typed in that moment never
    /// reached the shell.  Stopped here, a reader finishes the frame it
    /// started and reads no further; the rest stays in the socket for
    /// the next image.
    pub(crate) fn stop(&mut self) {
        let _ = (&self.stop_tx).write_all(&[0]);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn read_frames(mut reader: UnixStream, stop: &UnixStream, tx: Sender<SessionEvent>, generation: u64) {
    loop {
        if !crate::pty_reader::readable_before_stop(reader.as_raw_fd(), stop) {
            return;
        }
        match Frame::read_from(&mut reader) {
            Ok(Some(f)) => match f.msg_type {
                MsgType::KeyEvent => {
                    // The trailing window id is an L1↔L2 concern; a
                    // session belongs to a pane, and a pane does not
                    // care which window draws it.
                    if let Ok((w, _window_id)) = decode_key_event(&f.payload) {
                        let (e, m) = wire_to_event(w);
                        if tx.send(SessionEvent::Key(e, m)).is_err() {
                            break;
                        }
                    }
                }
                MsgType::GridResize => {
                    if let Ok((cols, rows)) = decode_grid_resize(&f.payload)
                        && tx.send(SessionEvent::Resize(cols, rows)).is_err() {
                            break;
                        }
                }
                MsgType::GridScroll => {
                    if let Ok(off) = decode_grid_scroll(&f.payload)
                        && tx.send(SessionEvent::Scroll(off)).is_err() {
                            break;
                        }
                }
                MsgType::GetSelectionText => {
                    if let Ok((seq, a, fo, bw)) = decode_get_selection_text(&f.payload)
                        && tx.send(SessionEvent::GetSelection(seq, a, fo, bw)).is_err() {
                            break;
                        }
                }
                MsgType::Paste => {
                    if let Ok(text) = decode_paste(&f.payload)
                        && tx.send(SessionEvent::Paste(text)).is_err() {
                            break;
                        }
                }
                MsgType::PaneHoldGrid => {
                    if let Ok((_sid, on)) =
                        marspot_term::shell_proto::decode_pane_hold_grid(&f.payload)
                        && tx.send(SessionEvent::HoldGrid(on)).is_err() {
                            break;
                        }
                }
                MsgType::PaneResetAttrs => {
                    if marspot_term::shell_proto::decode_pane_reset_attrs(&f.payload)
                        .is_ok()
                        && tx.send(SessionEvent::ResetAttrs).is_err()
                    {
                        break;
                    }
                }
                MsgType::PaneRenderMarkup => {
                    if let Ok((_sid, on)) =
                        marspot_term::shell_proto::decode_pane_render_markup(&f.payload)
                        && tx.send(SessionEvent::RenderMarkup(on)).is_err() {
                            break;
                        }
                }
                MsgType::InjectInput => {
                    if let Ok((_sid, bytes)) =
                        marspot_term::shell_proto::decode_inject_input(&f.payload)
                        && tx.send(SessionEvent::InjectInput(bytes)).is_err() {
                            break;
                        }
                }
                MsgType::PaneTerminalFocus => {
                    if let Ok((_sid, focused)) =
                        marspot_term::shell_proto::decode_pane_terminal_focus(&f.payload)
                        && tx.send(SessionEvent::TerminalFocus(focused)).is_err()
                    {
                        break;
                    }
                }
                MsgType::PaneResetMouseReporting => {
                    if marspot_term::shell_proto::decode_pane_reset_mouse_reporting(
                        &f.payload,
                    )
                    .is_ok()
                        && tx.send(SessionEvent::ResetMouseReporting).is_err()
                    {
                        break;
                    }
                }
                MsgType::SearchScrollback => {
                    if let Ok((query_id, case_sensitive, max_total, query)) =
                        decode_search_scrollback(&f.payload)
                        && tx
                            .send(SessionEvent::SearchRequest {
                                query_id,
                                case_sensitive,
                                max_total,
                                query,
                            })
                            .is_err()
                        {
                            break;
                        }
                }
                MsgType::SearchCancel => {
                    if let Ok(query_id) = decode_search_cancel(&f.payload)
                        && tx
                            .send(SessionEvent::SearchCancelRequested(query_id))
                            .is_err()
                        {
                            break;
                        }
                }
                _ => {}
            },
            Ok(None) | Err(_) => {
                let _ = tx.send(SessionEvent::CoreGone(generation));
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_term::shell_proto::encode_inject_input;
    use std::time::Duration;

    fn inject(text: &str) -> Frame {
        Frame::new(MsgType::InjectInput, encode_inject_input(0, text.as_bytes()))
    }

    #[test]
    fn a_stopped_reader_leaves_what_it_has_not_read_in_the_socket() {
        let (mut l2, l3) = UnixStream::pair().unwrap();
        // the half an execv would hand over
        let mut next_image = l3.try_clone().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut readers = ControlReaders::new().unwrap();
        readers.spawn(l3, tx, 0);

        inject("one").write_to(&mut l2).unwrap();
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            SessionEvent::InjectInput(b) => assert_eq!(b, b"one"),
            _ => panic!("not the injected input"),
        }
        // idle, blocked waiting for the next frame: stop must not hang
        readers.stop();

        inject("two").write_to(&mut l2).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err(), "a stopped reader read on");
        let f = Frame::read_from(&mut next_image).unwrap().expect("a frame");
        assert_eq!(f.msg_type, MsgType::InjectInput);
        assert_eq!(marspot_term::shell_proto::decode_inject_input(&f.payload).unwrap().1, b"two");
    }
}
