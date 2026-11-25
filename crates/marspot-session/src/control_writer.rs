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

use std::io::{self, Write};
use std::os::fd::{IntoRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use marspot_term::frame_writer::{BoundedWriter, Queued};
use marspot_term::shell_proto::{Frame, HEADER_LEN, MsgType};

/// What the writer thread puts on the wire.
enum Item {
    Frame(Frame),
    /// A `GridReady` poke.  The flag is the one `ControlWriter::poke`
    /// sets; it is cleared just before the poke is written, so a
    /// publish that finds it set happened before the core was woken and
    /// is read by the shm read that wake starts.
    Poke(Arc<AtomicBool>),
}

impl Queued for Item {
    fn queued_len(&self) -> usize {
        match self {
            Item::Frame(f) => f.wire_len(),
            Item::Poke(_) => HEADER_LEN,
        }
    }

    fn write_all_to(&self, sink: &mut dyn Write) -> io::Result<()> {
        match self {
            Item::Frame(f) => f.write_all_to(sink),
            Item::Poke(queued) => {
                queued.store(false, Ordering::SeqCst);
                Frame::new(MsgType::GridReady, Vec::new()).write_all_to(sink)
            }
        }
    }
}

/// L3's end of the control socket.  The queueing lives in
/// [`FrameWriter`]; this adds the fd bookkeeping the execv handoff
/// needs, which is specific to L3's self-update path.
pub struct ControlWriter {
    inner: BoundedWriter<Item>,
    /// A poke is queued and not yet written.  A core that stops reading
    /// would otherwise collect one twelve-byte poke per publish until
    /// the queue's cap, a third of a million of them, and every one but
    /// the first says nothing the first does not.
    poke_queued: Arc<AtomicBool>,
    /// The stream's fd, kept so the execv handoff can pass it on.
    fd: RawFd,
    /// Held only so the fd stays open for `fd`'s lifetime; the writer
    /// thread works from its own clone.
    stream: Option<UnixStream>,
}

impl ControlWriter {
    /// Take over `stream` for writing.  On clone failure the caller
    /// gets the stream back untouched so it can decide what to do
    /// rather than losing the connection.
    pub fn new(stream: UnixStream) -> Result<Self, (io::Error, UnixStream)> {
        let thread_half = match stream.try_clone() {
            Ok(c) => c,
            Err(e) => return Err((e, stream)),
        };
        let fd = {
            use std::os::fd::AsRawFd;
            stream.as_raw_fd()
        };
        Ok(Self {
            inner: BoundedWriter::new("l3-control-writer", thread_half, marspot_term::frame_writer::cap::CONTROL_SOCKET),
            poke_queued: Arc::new(AtomicBool::new(false)),
            fd,
            stream: Some(stream),
        })
    }

    /// Queue `frame` for L2.  Never blocks.  `false` = dropped.
    pub fn send(&mut self, frame: Frame) -> bool {
        self.inner.send(Item::Frame(frame))
    }

    /// Tell the core the grid changed.  Folded into a poke still
    /// waiting in the queue, which wakes the core to the same shm.
    pub fn poke(&mut self) {
        if self.poke_queued.swap(true, Ordering::SeqCst) {
            return;
        }
        if !self.inner.send(Item::Poke(Arc::clone(&self.poke_queued))) {
            self.poke_queued.store(false, Ordering::SeqCst);
        }
    }

    pub fn backlog(&self) -> usize {
        self.inner.backlog()
    }

    pub fn dropped_bytes(&self) -> usize {
        self.inner.dropped_bytes()
    }

    /// Surrender the underlying fd for the execv handoff.
    ///
    /// The writer thread's clone is not closed here: `execv` replaces
    /// the process image and terminates other threads without running
    /// destructors, so nothing gets a chance to close it.  Anything
    /// still queued is lost, which is what happened before this type
    /// existed too.
    pub fn into_raw_fd(mut self) -> RawFd {
        self.inner.close();
        match self.stream.take() {
            Some(s) => s.into_raw_fd(),
            None => self.fd,
        }
    }
}

#[cfg(test)]
#[path = "control_writer_tests.rs"]
mod tests;
