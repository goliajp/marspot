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

use marspot_term::shell_proto::{FrameReader, Seen};

use crate::SessionEvent;
use control_dispatch::dispatch;

#[path = "control_dispatch.rs"]
mod control_dispatch;

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
    let mut frames = FrameReader::new();
    loop {
        if !crate::pty_reader::readable_before_stop(reader.as_raw_fd(), stop) {
            return;
        }
        // one frame per check: a frame of a type this image does not
        // know is followed by none at all as often as not, and a
        // reader blocked for the one after it never sees a stop
        let go = frames.read_next(&mut reader, |seen| match seen {
            Seen::Unknown(_) => true,
            Seen::Frame(t, payload) => dispatch(t, payload, &tx, generation),
        });
        match go {
            Ok(Some(true)) => {}
            Ok(Some(false)) => break,
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
    use marspot_term::shell_proto::{Frame, MsgType};
    use marspot_term::shell_proto::{MAGIC, encode_inject_input};
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

    /// A frame this image does not know, and then nothing: a stop
    /// still ends the reader.  An image that blocked for the frame
    /// after the unknown one hung its update, for as long as the core
    /// sent nothing else.
    #[test]
    fn a_reader_past_an_unknown_frame_still_stops() {
        let (mut l2, l3) = UnixStream::pair().unwrap();
        let queued = l3.try_clone().unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut readers = ControlReaders::new().unwrap();
        readers.spawn(l3, tx, 0);

        let mut unknown = MAGIC.to_le_bytes().to_vec();
        unknown.extend_from_slice(&u32::MAX.to_le_bytes());
        unknown.extend_from_slice(&4u32.to_le_bytes());
        unknown.extend_from_slice(&[1, 2, 3, 4]);
        l2.write_all(&unknown).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while unread(&queued) > 0 {
            assert!(std::time::Instant::now() < deadline, "the reader never took the frame");
            std::thread::sleep(Duration::from_millis(1));
        }

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            readers.stop();
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(2)).is_ok(),
            "the reader blocked past the unknown frame and never saw the stop"
        );
        drop(l2);
    }

    fn unread(s: &UnixStream) -> libc::c_int {
        let mut n: libc::c_int = 0;
        // SAFETY: FIONREAD writes one int, on a socket the test owns.
        unsafe { libc::ioctl(s.as_raw_fd(), libc::FIONREAD, &mut n) };
        n
    }

    /// What is typed raises the flag the parser looks at between two
    /// slices, once it is queued for the loop.
    #[test]
    fn typed_input_is_flagged_for_the_parser() {
        use std::sync::atomic::Ordering;

        let (mut l2, l3) = UnixStream::pair().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut readers = ControlReaders::new().unwrap();
        readers.spawn(l3, tx, 0);
        crate::input::taken();

        inject("k").write_to(&mut l2).unwrap();
        assert!(matches!(rx.recv_timeout(Duration::from_secs(5)), Ok(SessionEvent::InjectInput(_))));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !crate::input::waiting().load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "the flag was never raised");
            std::thread::yield_now();
        }
        readers.stop();
    }
}
