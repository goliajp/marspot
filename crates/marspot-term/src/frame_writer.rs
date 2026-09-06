//! Non-blocking frame sink: a queue in front of a blocking writer.
//!
//! Every socket between the three layers is a plain blocking
//! `UnixStream`, and macOS gives `AF_UNIX`/`SOCK_STREAM` an 8 KiB send
//! buffer — measured, not assumed:
//!
//! ```text
//! AF_UNIX SOCK_STREAM: SO_SNDBUF=8192  SO_RCVBUF=8192
//! BLOCKED: write(2) parked after 8192 bytes
//! ```
//!
//! So any layer that writes frames from its event loop is one
//! slow-to-read peer away from parking that loop entirely.  L3 shipped
//! exactly that bug against L2; L2 has the mirror image against both
//! L1 and L3, where the blast radius is worse because L2 drives every
//! pane.
//!
//! The shape of the fix is the same everywhere, which is why it lives
//! here rather than being written a third time: hand the blocking call
//! to a thread and give the loop a queue.  Idle cost is zero — the
//! thread parks in `recv`.
//!
//! **Whole frames only.**  A frame is queued or refused as a unit and
//! written by a single owner, so a peer never sees a torn frame.  That
//! matters more than delivery: half a frame desynchronises the
//! protocol stream permanently, whereas a dropped frame costs one
//! poke, one reply, or one redraw.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::shell_proto::Frame;

/// Queue caps, one place.
///
/// Four call sites used to name this concept four different ways with
/// values 16× apart and a fresh justification at each — which is how you
/// end up unable to answer "how much can be in flight across the whole
/// system?" without grepping.  Sizing rule: hold the largest *legitimate*
/// burst the channel carries, and no more, so a wedged peer can't turn
/// the queue into an unbounded sink.
pub mod cap {
    /// L2↔L3 control sockets, both directions.  Must fit one
    /// `SelectionText` carrying a Cmd-A over deep scrollback, and one
    /// large paste going the other way.
    pub const CONTROL_SOCKET: usize = 4 * 1024 * 1024;
    /// L2→L1.  Small, steady traffic (surface acks, caret rects, pokes)
    /// — sized to ride out a busy AppKit loop, not to hold a burst.
    pub const L2_TO_L1: usize = 1024 * 1024;
    /// Keystrokes and injected input into one PTY.  A human cannot reach
    /// this; hitting it means the foreground process has genuinely
    /// stopped reading stdin.
    pub const PTY: usize = 256 * 1024;
}

/// One queued item.  Implementors say how much they weigh against the
/// cap and how to put themselves on the wire.
pub trait Queued: Send + 'static {
    /// Bytes this item accounts for against the queue cap.
    fn queued_len(&self) -> usize;
    /// Write the item in full.  Implementors own their own
    /// short-write handling — a PTY needs a retry loop, a framed
    /// protocol writes header+payload as a unit.
    fn write_all_to(&self, sink: &mut dyn Write) -> std::io::Result<()>;
}

impl Queued for Frame {
    fn queued_len(&self) -> usize {
        self.wire_len()
    }
    fn write_all_to(&self, sink: &mut dyn Write) -> std::io::Result<()> {
        // `&mut dyn Write` is itself `Write`, so re-borrow to satisfy
        // `write_to`'s `Sized` bound without making the trait generic
        // (which would stop it being object-safe).
        self.write_to(&mut { sink }).map(|_| ())
    }
}

/// A bounded queue in front of a thread that is allowed to block.
///
/// This is the shape every non-blocking writer in the tree needs, which
/// is the whole reason it is generic rather than copied: the PTY writer,
/// the L3→L2 control writer, and both L2 writers are the same machine
/// with a different item type.  (It was in fact hand-written four times
/// before this got extracted, under a comment claiming it existed so
/// nobody would write it a third time.)
pub struct BoundedWriter<T: Queued> {
    tx: Option<Sender<T>>,
    pending: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
    cap_bytes: usize,
}

/// Frame-carrying specialisation — the L1/L2/L3 protocol sockets.
pub type FrameWriter = BoundedWriter<Frame>;

impl<T: Queued> BoundedWriter<T> {
    /// Take over `sink`, writing from a thread named `name`.
    ///
    /// `cap_bytes` bounds what may sit queued before frames are
    /// refused.  Size it to the largest legitimate burst the channel
    /// carries — a selection reply can be megabytes, a poke is twelve
    /// bytes — while staying small enough that a wedged peer cannot
    /// turn the queue into an unbounded sink.
    pub fn new<W>(name: &str, mut sink: W, cap_bytes: usize) -> Self
    where
        W: Write + Send + 'static,
    {
        let (tx, rx): (Sender<T>, Receiver<T>) = mpsc::channel();
        let pending = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let pending_thread = Arc::clone(&pending);
        thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                // Ends when the owner drops its Sender.
                while let Ok(item) = rx.recv() {
                    let size = item.queued_len();
                    // A write error means the peer is gone.  Keep
                    // draining rather than exiting, so `pending` stays
                    // truthful and teardown happens through the normal
                    // EOF path instead of a queue that silently stops.
                    let _ = item.write_all_to(&mut sink);
                    pending_thread.fetch_sub(size, Ordering::SeqCst);
                }
            })
            .expect("spawn frame-writer thread");
        Self {
            tx: Some(tx),
            pending,
            dropped,
            cap_bytes,
        }
    }

    /// Queue `frame`.  Never blocks.
    ///
    /// `false` means the frame was dropped: either the queue is full
    /// (the peer has stopped reading) or the writer is gone.  Callers
    /// log; retrying into a full queue only drops again.
    pub fn send(&self, item: T) -> bool {
        let size = item.queued_len();
        let Some(tx) = self.tx.as_ref() else {
            return false;
        };
        if self.pending.load(Ordering::SeqCst) + size > self.cap_bytes {
            self.dropped.fetch_add(size, Ordering::SeqCst);
            return false;
        }
        self.pending.fetch_add(size, Ordering::SeqCst);
        if tx.send(item).is_err() {
            self.pending.fetch_sub(size, Ordering::SeqCst);
            return false;
        }
        true
    }

    /// Bytes queued but not yet accepted by the kernel.  Non-zero for
    /// more than a moment means the peer is not reading.
    pub fn backlog(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }

    /// Bytes refused so far because the queue was full.
    pub fn dropped_bytes(&self) -> usize {
        self.dropped.load(Ordering::SeqCst)
    }

    /// Stop accepting work and let the writer thread finish.  Used when
    /// the underlying fd is about to be handed to someone else.
    pub fn close(&mut self) {
        self.tx = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell_proto::MsgType;
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    /// The point of the type: a peer that stops reading must not park
    /// the caller.  macOS parks a blocking `write_all` after ~8 KiB, so
    /// queueing far past that has to stay fast.
    #[test]
    fn send_does_not_block_on_a_silent_peer() {
        let (a, _b) = UnixStream::pair().unwrap();
        // `_b` is never read — its buffer fills and stays full.
        let w = FrameWriter::new("test-writer", a, 4 * 1024 * 1024);
        let payload = vec![b'x'; 64 * 1024];
        let t0 = Instant::now();
        let mut queued = 0;
        for _ in 0..16 {
            if w.send(Frame::new(MsgType::SelectionText, payload.clone())) {
                queued += 1;
            }
        }
        let elapsed = t0.elapsed();
        assert_eq!(queued, 16);
        assert!(
            elapsed < Duration::from_secs(1),
            "send blocked for {elapsed:?}"
        );
        assert!(w.backlog() > 0, "expected a backlog against a silent peer");
    }

    #[test]
    fn queue_is_bounded_and_refuses_whole_frames() {
        let (a, _b) = UnixStream::pair().unwrap();
        let w = FrameWriter::new("test-writer", a, 1024 * 1024);
        let payload = vec![b'x'; 256 * 1024];
        let mut refused = 0;
        for _ in 0..32 {
            if !w.send(Frame::new(MsgType::SelectionText, payload.clone())) {
                refused += 1;
            }
        }
        assert!(refused > 0, "8 MiB accepted against a 1 MiB cap");
        assert!(w.backlog() <= 1024 * 1024);
        assert!(w.dropped_bytes() > 0);
    }

    /// A reading peer gets the frames, in order, byte-identical.
    #[test]
    fn frames_reach_a_reading_peer_in_order() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let w = FrameWriter::new("test-writer", a, 1024 * 1024);
        for i in 0u8..4 {
            assert!(w.send(Frame::new(MsgType::GridReady, vec![i; 8])));
        }
        b.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        // Read until every payload has shown up (or we run out of
        // patience).  Don't compute an expected byte count here — that
        // just re-derives the header layout in the test and gets it
        // wrong.
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        let all_present =
            |bytes: &[u8]| (0u8..4).all(|i| bytes.windows(8).any(|win| win == [i; 8]));
        while Instant::now() < deadline && !all_present(&got) {
            let mut buf = [0u8; 512];
            match b.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        let mut last = None;
        for i in 0u8..4 {
            let at = got
                .windows(8)
                .position(|win| win == [i; 8])
                .unwrap_or_else(|| panic!("frame {i} never arrived"));
            if let Some(prev) = last {
                assert!(at > prev, "frame {i} arrived out of order");
            }
            last = Some(at);
        }
    }
}
