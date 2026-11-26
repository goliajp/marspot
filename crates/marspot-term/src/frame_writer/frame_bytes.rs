//! The frame-carrying writer, queued as bytes rather than as frames.
//!
//! A queue of owned frames costs one payload allocation per frame and a
//! channel block every few dozen, which is the one cost a keystroke on its
//! way to a session must not pay.  Queued as bytes in a buffer the writer
//! thread swaps with its own, nothing is allocated once both buffers have
//! reached the size of the traffic.

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;

use crate::shell_proto::{Frame, HEADER_LEN, MsgType, frame_header};

/// A buffer the writer thread kept after a burst larger than this is
/// given back: a multi-megabyte selection reply must not stay resident
/// in every pane's writer.
const KEEP_CAPACITY: usize = 64 * 1024;

/// What each of the two buffers starts with: some sixty keystrokes or
/// pokes, so ordinary traffic never grows them, even in a burst the
/// writer thread has not caught up with.
const START_CAPACITY: usize = 2 * 1024;

struct Queue {
    bytes: Vec<u8>,
    open: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
    /// Queued plus being written.
    pending: AtomicUsize,
    dropped: AtomicUsize,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// A bounded queue of frames in front of a thread that is allowed to
/// block on the socket.  Whole frames only: a frame is queued or refused
/// as a unit, and bytes leave in the order frames were queued.
pub struct FrameWriter {
    shared: Arc<Shared>,
    cap_bytes: usize,
}

impl FrameWriter {
    /// Take over `sink`, writing from a thread named `name`; at most
    /// `cap_bytes` may wait before frames are refused.
    pub fn new<W>(name: &str, mut sink: W, cap_bytes: usize) -> Self
    where
        W: Write + Send + 'static,
    {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                bytes: Vec::with_capacity(START_CAPACITY),
                open: true,
            }),
            ready: Condvar::new(),
            pending: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
        });
        let theirs = Arc::clone(&shared);
        thread::Builder::new()
            .name(name.to_string())
            .spawn(move || drain(&theirs, &mut sink))
            .expect("spawn frame-writer thread");
        Self { shared, cap_bytes }
    }

    /// Queue `frame`.  Never blocks; `false` when it was refused (the
    /// queue is full, or the writer is closed).
    pub fn send(&self, frame: Frame) -> bool {
        self.send_with(frame.msg_type, |out| out.extend_from_slice(&frame.payload))
    }

    /// Queue a frame whose payload `encode` appends to the buffer it is
    /// given, which is the queue itself: nothing is copied or allocated
    /// on the way.  Never blocks; `false` when the frame was refused.
    pub fn send_with(&self, msg_type: MsgType, encode: impl FnOnce(&mut Vec<u8>)) -> bool {
        let mut q = self.shared.lock();
        if !q.open {
            return false;
        }
        let start = q.bytes.len();
        q.bytes.extend_from_slice(&[0; HEADER_LEN]);
        encode(&mut q.bytes);
        let size = q.bytes.len() - start;
        if self.shared.pending.load(Ordering::SeqCst) + size > self.cap_bytes {
            q.bytes.truncate(start);
            self.shared.dropped.fetch_add(size, Ordering::SeqCst);
            return false;
        }
        q.bytes[start..start + HEADER_LEN]
            .copy_from_slice(&frame_header(msg_type, size - HEADER_LEN));
        self.shared.pending.fetch_add(size, Ordering::SeqCst);
        drop(q);
        self.shared.ready.notify_one();
        true
    }

    /// Bytes queued but not yet accepted by the kernel.
    pub fn backlog(&self) -> usize {
        self.shared.pending.load(Ordering::SeqCst)
    }

    /// Bytes refused so far because the queue was full.
    pub fn dropped_bytes(&self) -> usize {
        self.shared.dropped.load(Ordering::SeqCst)
    }

    /// Stop accepting frames; the thread writes what is queued and ends.
    /// Used when the fd is about to be handed to someone else.
    pub fn close(&mut self) {
        self.shared.lock().open = false;
        self.shared.ready.notify_one();
    }
}

impl Drop for FrameWriter {
    fn drop(&mut self) {
        self.close();
    }
}

fn drain(shared: &Shared, sink: &mut dyn Write) {
    let mut out = Vec::with_capacity(START_CAPACITY);
    loop {
        {
            let mut q = shared.lock();
            while q.bytes.is_empty() && q.open {
                q = shared.ready.wait(q).unwrap_or_else(|p| p.into_inner());
            }
            if q.bytes.is_empty() {
                return;
            }
            std::mem::swap(&mut q.bytes, &mut out);
        }
        // a write error means the peer is gone; keep draining so
        // `pending` stays truthful and teardown runs through EOF
        let _ = sink.write_all(&out);
        shared.pending.fetch_sub(out.len(), Ordering::SeqCst);
        out.clear();
        if out.capacity() > KEEP_CAPACITY {
            out = Vec::with_capacity(START_CAPACITY);
        }
    }
}
