//! Non-blocking write half of the L3 → L2 control socket.
//!
//! **Why this exists.**  The control stream is a plain blocking
//! `UnixStream`, and macOS gives an `AF_UNIX`/`SOCK_STREAM` socket an
//! 8 KiB send buffer by default — measured, not assumed:
//!
//! ```text
//! AF_UNIX SOCK_STREAM: SO_SNDBUF=8192  SO_RCVBUF=8192
//! BLOCKED: write(2) parked after 8192 bytes
//! ```
//!
//! So if L2 stops draining its end for any reason, the very next frame
//! L3 writes past that 8 KiB parks the **entire L3 main loop** inside
//! `write_all`: no PTY pumping, no key handling, no snapshots, pane
//! frozen until L2 reads again.  A `SelectionText` reply carrying a
//! Cmd-A selection over deep scrollback is megabytes on its own, so it
//! blows through the buffer in a single frame every time.
//!
//! This is the third instance of one bug shape found in a day — after
//! the blocking PTY write and the 50 MiB bytelog compaction — and the
//! answer is the same: give the blocking call its own thread and leave
//! the loop with a queue it can hand work to.  Idle cost is zero (the
//! thread parks in `recv`).
//!
//! Frames are queued whole, so a dropped frame is a *whole* frame and
//! the peer never sees a torn one — which matters more than delivery
//! here, since a half-written frame would desynchronise the protocol
//! stream permanently.

use std::io;
use std::os::fd::{IntoRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use marspot_term::shell_proto::Frame;

/// Bytes allowed to sit queued for L2 before frames start being
/// dropped.  Generous next to the kernel's 8 KiB, because a legitimate
/// burst (a big `SelectionText`) must fit; small enough that a wedged
/// L2 can't turn this into an unbounded sink.
const QUEUE_CAP_BYTES: usize = 4 * 1024 * 1024;

pub struct ControlWriter {
    tx: Option<Sender<Frame>>,
    pending: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
    /// The stream's fd, kept so the execv handoff can pass it on.
    fd: RawFd,
    /// Held only so the fd stays open for `fd`'s lifetime; the writer
    /// thread works from its own clone.
    _stream: Option<UnixStream>,
}

impl ControlWriter {
    /// Take over `stream` for writing.  On clone failure the caller
    /// gets the stream back untouched so it can fall back to writing
    /// inline rather than losing the connection.
    pub fn new(stream: UnixStream) -> Result<Self, (io::Error, UnixStream)> {
        let thread_half = match stream.try_clone() {
            Ok(c) => c,
            Err(e) => return Err((e, stream)),
        };
        let fd = {
            use std::os::fd::AsRawFd;
            stream.as_raw_fd()
        };
        let (tx, rx): (Sender<Frame>, Receiver<Frame>) = mpsc::channel();
        let pending = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let pending_thread = Arc::clone(&pending);
        thread::Builder::new()
            .name("l3-control-writer".into())
            .spawn(move || {
                let mut sock = thread_half;
                // Ends when the session drops its Sender.
                while let Ok(frame) = rx.recv() {
                    let size = frame.wire_len();
                    // A failed write means L2 went away; the reader
                    // side will see EOF and tear down.  Keep draining
                    // so `pending` stays truthful.
                    let _ = frame.write_to(&mut sock);
                    pending_thread.fetch_sub(size, Ordering::SeqCst);
                }
            })
            .expect("spawn l3-control-writer thread");
        Ok(Self {
            tx: Some(tx),
            pending,
            dropped,
            fd,
            _stream: Some(stream),
        })
    }

    /// Queue `frame` for L2.  Never blocks.
    ///
    /// `false` means the frame was dropped because L2 has not been
    /// reading long enough to fill the queue — the caller logs it.
    /// There is no retry: retrying into a full queue just drops again.
    pub fn send(&mut self, frame: Frame) -> bool {
        let size = frame.wire_len();
        let Some(tx) = self.tx.as_ref() else {
            return false;
        };
        if self.pending.load(Ordering::SeqCst) + size > QUEUE_CAP_BYTES {
            self.dropped.fetch_add(size, Ordering::SeqCst);
            return false;
        }
        self.pending.fetch_add(size, Ordering::SeqCst);
        if tx.send(frame).is_err() {
            self.pending.fetch_sub(size, Ordering::SeqCst);
            return false;
        }
        true
    }

    /// Bytes queued for L2 but not yet accepted by the kernel.
    pub fn backlog(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }

    /// Bytes dropped so far because the queue was full.
    pub fn dropped_bytes(&self) -> usize {
        self.dropped.load(Ordering::SeqCst)
    }

    /// Surrender the underlying fd for the execv handoff.
    ///
    /// The writer thread's clone is not closed here: `execv` replaces
    /// the process image and terminates other threads without running
    /// destructors, so nothing gets a chance to close it.  Anything
    /// still queued is lost, which is what happened before this type
    /// existed too.
    pub fn into_raw_fd(mut self) -> RawFd {
        // Drop the Sender first so the thread stops taking new work.
        self.tx = None;
        match self._stream.take() {
            Some(s) => s.into_raw_fd(),
            None => self.fd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_term::shell_proto::MsgType;
    use std::io::Read;
    use std::time::{Duration, Instant};

    /// The whole point: a peer that stops reading must not be able to
    /// park the caller.
    ///
    /// macOS gives these sockets an 8 KiB send buffer, so a blocking
    /// `write_all` wedges after ~8 KiB.  Queue well past that and
    /// require the calls to stay fast.
    #[test]
    fn send_does_not_block_when_the_peer_stops_reading() {
        let (a, _b) = UnixStream::pair().unwrap();
        // `_b` is never read from — its receive buffer fills and stays
        // full, which is what parks a blocking writer.
        let mut w = ControlWriter::new(a).map_err(|(e, _)| e).unwrap();

        let payload = vec![b'x'; 64 * 1024];
        let t0 = Instant::now();
        let mut queued = 0;
        for _ in 0..16 {
            if w.send(Frame::new(MsgType::SelectionText, payload.clone())) {
                queued += 1;
            }
        }
        let elapsed = t0.elapsed();

        assert_eq!(queued, 16, "1 MiB should fit under the 4 MiB cap");
        assert!(
            elapsed < Duration::from_secs(1),
            "send blocked for {elapsed:?} — the queue is not absorbing a silent peer"
        );
        // Proof the peer really isn't draining: the kernel took at most
        // its 8 KiB buffer, so the rest must still be queued.
        assert!(w.backlog() > 0, "expected a backlog against a silent peer");
    }

    /// Past the cap, whole frames are refused — never half-written,
    /// which would desync the protocol stream.
    #[test]
    fn queue_is_bounded_and_drops_whole_frames() {
        let (a, _b) = UnixStream::pair().unwrap();
        let mut w = ControlWriter::new(a).map_err(|(e, _)| e).unwrap();
        let payload = vec![b'x'; 512 * 1024];
        let mut refused = 0;
        for _ in 0..32 {
            if !w.send(Frame::new(MsgType::SelectionText, payload.clone())) {
                refused += 1;
            }
        }
        assert!(refused > 0, "16 MiB was accepted against a 4 MiB cap");
        assert!(w.backlog() <= QUEUE_CAP_BYTES);
        assert!(w.dropped_bytes() > 0);
    }

    /// Frames that fit arrive intact and in order.
    #[test]
    fn frames_arrive_in_order() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let mut w = ControlWriter::new(a).map_err(|(e, _)| e).unwrap();
        for i in 0u8..4 {
            assert!(w.send(Frame::new(MsgType::GridReady, vec![i; 4])));
        }
        // Give the writer thread a moment to drain.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = Vec::new();
        b.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        while Instant::now() < deadline && got.len() < 4 {
            let mut buf = [0u8; 256];
            match b.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
            if w.backlog() == 0 && !got.is_empty() {
                break;
            }
        }
        assert!(!got.is_empty(), "nothing arrived");
        // Payload bytes appear in send order within the stream.
        let mut last = None;
        for i in 0u8..4 {
            if let Some(at) = got.windows(4).position(|win| win == [i; 4]) {
                if let Some(prev) = last {
                    assert!(at > prev, "frame {i} arrived out of order");
                }
                last = Some(at);
            }
        }
    }
}
