//! The connection thread's side of a web pane's input: what the main
//! thread queued goes out on the display channel when the thread is
//! woken (the latest size as a new surface, then whether the pane is
//! shown, then the queue), and what the page says back waits for the
//! main thread.  A content process agreed on a display channel version
//! before input is sent none, and what it says of input is not taken.
//! How long a scroll took to show, from the wheel event to a commit
//! that drew it, is logged in a sandbox, where the scroll gauge reads it.

use std::os::fd::{FromRawFd, OwnedFd};
use std::time::Instant;

use madcawl_wire::display::kind;
use madcawl_wire::input::{self, Scroll, SINCE};
use madcawl_wire::Frame;

use super::frames::BAD_IN_A_ROW;
use super::gpu::Device;
use super::input::Out;
use super::link::{attach, failed, End, Link, Live};
use super::Shared;

/// What the thread is sending, between wakes.
pub(super) struct Sending {
    pub(super) session: u64,
    /// The display channel version agreed with the content process
    version: u32,
    outs: Vec<Out>,
    /// When the oldest wheel event not yet drawn came
    scroll_since: Option<Instant>,
    /// Frames about input in a row that did not decode
    bad: u32,
}

impl Sending {
    pub(super) fn new(session: u64, version: u32) -> Sending {
        Sending { session, version, outs: Vec::new(), scroll_since: None, bad: 0 }
    }

    fn speaks_input(&self) -> bool {
        self.version >= SINCE
    }

    /// Send what the main thread left: the latest size, whether the
    /// pane is shown, and the queue.
    pub(super) fn send(&mut self, ch: &mut Link, live: &mut Live, device: &Device, shared: &Shared) -> Result<(), End> {
        let (resize, visible) = {
            let mut g = shared.lock();
            self.outs.extend(g.out.drain(..));
            (g.resize.take(), g.visible.take())
        };
        if let Some((px, scale)) = resize
            && let Some((_, px)) = super::frames::page_size(px.0, px.1, scale)
        {
            let epoch = live.commits.surface().epoch + 1;
            *live = attach(ch, px, scale, epoch, device)?;
        }
        if !self.speaks_input() {
            self.outs.clear();
            return Ok(());
        }
        if let Some(on) = visible {
            ch.send(&input::visible(on), &[])?;
        }
        for i in 0..self.outs.len() {
            let f = match &self.outs[i] {
                Out::Scroll(dx, dy, at) => {
                    self.scroll_since.get_or_insert(*at);
                    Scroll { dx: *dx, dy: *dy }.frame()
                }
                Out::Pointer(p) => p.frame(),
                Out::Key(k) => input::key(k.key.as_str(), k.text.as_str(), k.mods, k.repeat),
                Out::Copy(seq) => input::copy(*seq),
                Out::Paste(text) => input::paste(text),
            };
            ch.send(&f, &[])?;
        }
        self.outs.clear();
        Ok(())
    }

    /// A commit was shown.
    pub(super) fn shown(&mut self) {
        if let Some(at) = self.scroll_since.take()
            && super::dev_url().is_some()
        {
            crate::lx_info!("web_pane.scroll", "a scroll was drawn", session = self.session, latency_us = at.elapsed().as_micros() as u64);
        }
    }

    /// A frame from the content process that is not a commit: the
    /// pointer's shape, copied text or a link to open, each for the main
    /// thread.  The page is untrusted: a frame that does not decode is
    /// passed over and counted, and `BAD_IN_A_ROW` of them end the
    /// channel, and with it the content process, as bad commits do.
    pub(super) fn page_said(&mut self, f: &Frame, shared: &Shared) -> Result<(), End> {
        if !self.speaks_input() || !matches!(f.kind, kind::CURSOR | kind::COPIED | kind::OPEN) {
            return Ok(());
        }
        let mut g = shared.lock();
        let ok = match f.kind {
            kind::CURSOR => input::cursor_of(f).map(|c| g.said.cursor = Some(c)),
            kind::COPIED => input::copied_of(f).map(|c| g.said.copied = Some(c)),
            _ => input::open_url(f).map(|u| g.said.open = Some(u)),
        };
        drop(g);
        match ok {
            Ok(()) => {
                self.bad = 0;
                (shared.notify)();
                Ok(())
            }
            Err(e) => {
                self.bad += 1;
                crate::lx_warn!("web_pane.bad_frame", &e.to_string(), session = self.session, in_a_row = self.bad);
                match self.bad >= BAD_IN_A_ROW {
                    true => Err(failed("the content process kept sending bad frames", e)),
                    false => Ok(()),
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Shared {
        Shared { inner: std::sync::Mutex::new(Default::default()), notify: Box::new(|| {}) }
    }

    #[test]
    fn web_pane_bad_input_frames_three_in_a_row_end_the_channel() {
        let s = shared();
        let mut sending = Sending::new(1, 3);
        let bad = Frame::new(kind::CURSOR);
        assert!(sending.page_said(&bad, &s).is_ok());
        assert!(sending.page_said(&bad, &s).is_ok());
        assert!(sending.page_said(&input::cursor(input::Cursor::Text), &s).is_ok(), "a good one starts the count again");
        assert!(sending.page_said(&bad, &s).is_ok());
        assert!(sending.page_said(&Frame::new(kind::COPIED), &s).is_ok());
        assert!(matches!(sending.page_said(&Frame::new(kind::OPEN), &s), Err(End::Failed(_))));
    }

    #[test]
    fn web_pane_a_version_2_peer_is_heard_on_nothing_of_input() {
        let s = shared();
        let mut sending = Sending::new(1, 2);
        for _ in 0..5 {
            assert!(sending.page_said(&Frame::new(kind::CURSOR), &s).is_ok());
        }
        assert!(sending.page_said(&input::open("https://example.com"), &s).is_ok());
        assert!(s.lock().said.is_empty());
    }
}
