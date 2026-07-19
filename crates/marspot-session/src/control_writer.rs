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

use marspot_term::frame_writer::FrameWriter;
use marspot_term::shell_proto::Frame;

/// Bytes allowed to sit queued for L2 before frames start being
/// dropped.  Generous next to the kernel's 8 KiB, because a legitimate
/// burst (a big `SelectionText`) must fit; small enough that a wedged
/// L2 can't turn this into an unbounded sink.
const QUEUE_CAP_BYTES: usize = 4 * 1024 * 1024;

/// L3's end of the control socket.  The queueing lives in
/// [`FrameWriter`]; this adds the fd bookkeeping the execv handoff
/// needs, which is specific to L3's self-update path.
pub struct ControlWriter {
    inner: FrameWriter,
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
            inner: FrameWriter::new("l3-control-writer", thread_half, QUEUE_CAP_BYTES),
            fd,
            stream: Some(stream),
        })
    }

    /// Queue `frame` for L2.  Never blocks.  `false` = dropped.
    pub fn send(&mut self, frame: Frame) -> bool {
        self.inner.send(frame)
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
