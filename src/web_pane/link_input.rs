//! The connection thread's side of a web pane's input: what the main
//! thread queued goes out on the display channel when the thread is
//! woken (a new size as a new surface), and what the page says back
//! waits for the main thread.  How long a scroll took to show, from the
//! wheel event to a commit that drew it, is logged in a sandbox, where
//! the scroll gauge reads it.

use std::os::fd::{FromRawFd, OwnedFd};
use std::time::Instant;

use madcawl_wire::display::kind;
use madcawl_wire::input::{self, Scroll};
use madcawl_wire::Frame;

use super::gpu::Device;
use super::input::Out;
use super::link::{attach, End, Link, Live};
use super::Shared;

/// What the thread is sending, between wakes.
#[derive(Default)]
pub(super) struct Sending {
    outs: Vec<Out>,
    /// When the oldest wheel event not yet drawn came
    scroll_since: Option<Instant>,
}

impl Sending {
    /// Send what the main thread queued.
    pub(super) fn send(&mut self, ch: &Link, live: &mut Live, device: &Device, shared: &Shared) -> Result<(), End> {
        self.outs.extend(shared.lock().out.drain(..));
        for o in self.outs.drain(..) {
            let f = match o {
                Out::Scroll(dx, dy, at) => {
                    self.scroll_since.get_or_insert(at);
                    Scroll { dx, dy }.frame()
                }
                Out::Pointer(p) => p.frame(),
                Out::Key(k) => k.frame(),
                Out::Copy => input::copy(),
                Out::Paste(text) => input::paste(&text),
                Out::Visible(on) => input::visible(on),
                Out::Resize(px, scale) => {
                    let Some((_, px)) = super::frames::page_size(px.0, px.1, scale) else { continue };
                    let epoch = live.commits.surface().epoch + 1;
                    *live = attach(ch, px, scale, epoch, device)?;
                    continue;
                }
            };
            ch.send(&f, &[])?;
        }
        Ok(())
    }

    /// A commit was shown.
    pub(super) fn shown(&mut self, session: u64) {
        if let Some(at) = self.scroll_since.take()
            && super::dev_url().is_some()
        {
            crate::lx_info!("web_pane.scroll", "a scroll was drawn", session = session, latency_us = at.elapsed().as_micros() as u64);
        }
    }
}

/// A frame from the content process that is not a commit: the pointer's
/// shape, copied text or a link to open, each for the main thread.  The
/// page is untrusted: a frame that does not decode is passed over.
pub(super) fn page_said(f: &Frame, shared: &Shared, session: u64) {
    let mut g = shared.lock();
    let ok = match f.kind {
        kind::CURSOR => input::cursor_of(f).map(|c| g.said.cursor = Some(c)),
        kind::COPIED => input::copied_text(f).map(|t| g.said.copied = Some(t)),
        kind::OPEN => input::open_url(f).map(|u| g.said.open = Some(u)),
        _ => return,
    };
    drop(g);
    match ok {
        Ok(()) => (shared.notify)(),
        Err(e) => crate::lx_warn!("web_pane.bad_frame", &e.to_string(), session = session),
    }
}

/// A pipe whose ends do not block and are not inherited.
pub(super) fn pipe() -> Option<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: room for two descriptors
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    for fd in fds {
        // SAFETY: descriptors this process just made
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
        }
    }
    // SAFETY: as above, owned from here on
    Some(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}
