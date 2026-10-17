//! The thread that reads a pane's PTY output for the main loop.

use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};

use marspot_term::pty::Pty;

/// Chunk size.  Bigger than the parser's typical 4 KiB burst because
/// `cat bigfile` happily fills 64 KiB in a single read, and the kernel
/// returns whatever's queued -- no benefit to capping.
const READ_BUF_BYTES: usize = 64 * 1024;

pub(crate) struct PtyReader {
    stop: UnixStream,
    thread: JoinHandle<()>,
}

impl PtyReader {
    /// Queue what the PTY gives on `tx`, calling `wake` after each
    /// chunk; at the PTY's end, set `exited` and wake once more.
    pub(crate) fn spawn<W>(
        id: u64,
        pty: Arc<Pty>,
        exited: Arc<AtomicBool>,
        tx: Sender<Vec<u8>>,
        wake: W,
    ) -> Self
    where
        W: Fn() + Send + 'static,
    {
        let (stop, stop_rx) = UnixStream::pair().expect("a socket pair to stop the pty reader with");
        let thread = thread::Builder::new()
            .name(format!("l3-pty-reader-{id}"))
            .spawn(move || {
                let mut buf = vec![0u8; READ_BUF_BYTES];
                loop {
                    if !readable_before_stop(pty.raw_master(), &stop_rx) {
                        break;
                    }
                    let n = match pty.read_shared(&mut buf) {
                        Ok(n) => n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        // macOS gives EIO when the slave side fully closes
                        Err(_) => 0,
                    };
                    if n == 0 {
                        exited.store(true, Ordering::SeqCst);
                        wake();
                        break;
                    }
                    if tx.send(buf[..n].to_vec()).is_err() {
                        // main loop is gone -- stop forwarding
                        break;
                    }
                    wake();
                }
            })
            .expect("spawn l3-pty-reader thread");
        Self { stop, thread }
    }

    /// Stop between two reads and wait for the thread.  A chunk it has
    /// read is queued; what it has not read stays in the PTY.
    pub(crate) fn stop(self) {
        let _ = (&self.stop).write_all(&[0]);
        let _ = self.thread.join();
    }
}

/// Wait until `fd` has something to read, data or its end; `false`
/// once `stop` has, whatever `fd` holds.
pub(crate) fn readable_before_stop(fd: RawFd, stop: &UnixStream) -> bool {
    let mut fds = [
        libc::pollfd { fd, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: stop.as_raw_fd(), events: libc::POLLIN, revents: 0 },
    ];
    loop {
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            // the read reports it
            return true;
        }
        if fds[1].revents != 0 {
            return false;
        }
        if fds[0].revents != 0 {
            return true;
        }
    }
}
