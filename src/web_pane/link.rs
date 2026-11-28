//! A web pane's connection thread: the host connection and the display
//! channel of one pane.  It says hello to the host as the display end,
//! opens the page, says hello on the display channel it is handed, makes
//! two shared-memory buffers with a texture over each and sends them;
//! from then on it shows each commit that passes the checks and hands a
//! buffer back once a frame showing a newer one has finished.
//!
//! It blocks in `poll` on its sockets and the pane's wake pipe with no
//! timeout, so an idle pane costs nothing; the pane closing closes the
//! pipe, which ends the thread, its connections and, with them, the
//! page on the host's side.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;

use madcawl_wire::display::{self, Surface};
use madcawl_wire::host::{self, PaneOpen, PaneOpened};
use madcawl_wire::{os, Frame, FrameError, Hello, MAX_FRAME};

use super::frames::{hello_answer, Commits, Verdict};
use super::link_input::{page_said, Sending};
use super::gpu::{self, Device, Texture};
use super::{Note, Shared};

/// What the thread is asked to open.
pub struct Open {
    pub socket: PathBuf,
    pub session: u64,
    pub url: String,
    /// The page, CSS px, and its buffers, device pixels
    pub css: (u32, u32),
    pub px: (u32, u32),
    pub scale: u32,
}

/// Why the thread stopped.
pub(super) enum End {
    /// The pane closed its end of the wake pipe
    Closed,
    Failed(Note),
}

pub(super) fn failed(what: &str, e: impl std::fmt::Display) -> End {
    End::Failed(Note::Failed(format!("{what}: {e}")))
}

/// Frames on a socket, with the descriptors that come with them.
pub(super) struct Link {
    pub(super) s: UnixStream,
    buf: Vec<u8>,
    fds: Vec<OwnedFd>,
}

impl Link {
    /// Writes never block: a side that stops reading would otherwise
    /// hold this thread in `sendmsg`, deaf to the wake pipe. Reads come
    /// only after `poll` says there is something.
    fn new(s: UnixStream) -> Result<Link, End> {
        s.set_nonblocking(true).map_err(|e| failed("the socket", e))?;
        Ok(Link { s, buf: Vec::new(), fds: Vec::new() })
    }

    /// A frame that does not fit in the socket's buffer now ends the
    /// link: the other side has stopped reading.
    pub(super) fn send(&self, f: &Frame, fds: &[BorrowedFd]) -> Result<(), End> {
        let mut out = Vec::new();
        f.encode(&mut out);
        os::send(&self.s, &out, fds).map_err(|e| match e.kind() {
            io::ErrorKind::WouldBlock => failed("writing", "the other side stopped reading"),
            _ => failed("writing", e),
        })
    }

    /// A whole frame already read, if there is one.
    fn ready(&mut self) -> Result<Option<Frame>, End> {
        match Frame::decode(&self.buf) {
            Ok((f, used)) => {
                self.buf.drain(..used);
                Ok(Some(f))
            }
            Err(FrameError::Short) => Ok(None),
            Err(e) => Err(failed("a bad frame", format!("{e:?}"))),
        }
    }

    /// One read of what is there; the other side is untrusted, so what
    /// it may make this side hold is bounded.
    fn fill(&mut self) -> Result<(), End> {
        let mut chunk = [0u8; 16384];
        let n = os::recv(&self.s, &mut chunk, &mut self.fds).map_err(|e| failed("reading", e))?;
        if n == 0 {
            return Err(failed("reading", "the other side hung up"));
        }
        if self.buf.len() + n > MAX_FRAME + 4 || self.fds.len() > os::MAX_FDS {
            return Err(failed("reading", "more than a frame's worth"));
        }
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    /// The next frame, waiting for it as long as the pane is open.
    fn next(&mut self, wake: RawFd) -> Result<Frame, End> {
        loop {
            if let Some(f) = self.ready()? {
                return Ok(f);
            }
            let [readable, woken] = wait([self.s.as_raw_fd(), wake])?;
            if woken {
                drain(wake)?;
            }
            if readable {
                self.fill()?;
            }
        }
    }
}

/// Which of `fds` can be read (or have hung up), blocking until one can.
fn wait<const N: usize>(fds: [RawFd; N]) -> Result<[bool; N], End> {
    let mut p = fds.map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
    loop {
        // SAFETY: `p` is N pollfds that live for the call
        let r = unsafe { libc::poll(p.as_mut_ptr(), N as libc::nfds_t, -1) };
        if r >= 0 {
            return Ok(p.map(|x| x.revents != 0));
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(failed("waiting", e));
        }
    }
}

/// Empty the wake pipe; `Closed` once the pane has closed its end.
fn drain(wake: RawFd) -> Result<(), End> {
    let mut b = [0u8; 64];
    // SAFETY: reads into a buffer of that length; the pipe does not block
    let n = unsafe { libc::read(wake, b.as_mut_ptr().cast(), b.len()) };
    match n {
        0 => Err(End::Closed),
        _ => Ok(()),
    }
}

pub fn run(o: Open, device: Device, shared: Arc<Shared>, wake: OwnedFd) {
    if let Err(End::Failed(note)) = serve(&o, &device, &shared, wake.as_raw_fd()) {
        crate::lx_warn!("web_pane.ended", &note.to_string(), session = o.session);
        shared.set_note(note);
    }
}

fn serve(o: &Open, device: &Device, shared: &Shared, wake: RawFd) -> Result<(), End> {
    let s = UnixStream::connect(&o.socket).map_err(|e| failed("the madcawl host is not reachable", e))?;
    let mut host_link = Link::new(s)?;
    let hello = Hello { range: host::RANGE, role: Some(host::role::DISPLAY), version: None };
    host_link.send(&hello.frame(host::kind::HELLO), &[])?;
    let answer = host_link.next(wake)?;
    hello_answer(&answer, host::kind::HELLO, host::RANGE).map_err(|u| End::Failed(Note::Unspoken("host", host::RANGE, u)))?;
    let open = PaneOpen { session: o.session, url: o.url.clone(), width: o.css.0, height: o.css.1, scale: o.scale };
    host_link.send(&open.frame(), &[])?;
    let answer = host_link.next(wake)?;
    let opened = match PaneOpened::from_frame(&answer) {
        Ok(p) => p,
        Err(e) => {
            let why = host::refusal(&answer).map_or_else(|| e.to_string(), |(code, text)| format!("{text} ({code})"));
            return Err(failed("the host did not open the page", why));
        }
    };
    if host_link.fds.len() != 1 {
        return Err(failed("the host's answer", "no display channel came with it"));
    }
    let mut ch = Link::new(UnixStream::from(host_link.fds.remove(0)))?;
    crate::lx_event!("WEB_PANE_OPENED", "the host opened the page", session = o.session, page = opened.page, content_pid = opened.pid);
    let dh = Hello { range: display::RANGE, role: None, version: None };
    ch.send(&dh.frame(display::kind::HELLO), &[])?;
    let answer = ch.next(wake)?;
    hello_answer(&answer, display::kind::HELLO, display::RANGE)
        .map_err(|u| End::Failed(Note::Unspoken("content process", display::RANGE, u)))?;
    let live = attach(&ch, o.px, o.scale, 1, device)?;
    show(o.session, &mut host_link, &mut ch, live, device, shared, wake)
}

/// A surface the content process draws into, and a texture over each
/// of its buffers.
pub(super) struct Live {
    pub(super) textures: Vec<Texture>,
    pub(super) commits: Commits,
}

/// Make a surface's two buffers of `px` device pixels, a texture over
/// each, and send them.
pub(super) fn attach(ch: &Link, px: (u32, u32), scale: u32, epoch: u64, device: &Device) -> Result<Live, End> {
    let (mut fds, mut textures, mut buffers) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..display::MAX_BUFFERS {
        let (fd, m, b) = gpu::make_buffer(px.0, px.1).map_err(|e| failed("no shared memory", e))?;
        textures.push(gpu::wrap(device, m, &b).map_err(|e| failed("no texture", e))?);
        fds.push(fd);
        buffers.push(b);
    }
    let surface = Surface { epoch, scale, buffers };
    let borrowed: Vec<_> = fds.iter().map(|f| f.as_fd()).collect();
    ch.send(&surface.frame(), &borrowed)?;
    Ok(Live { textures, commits: Commits::new(surface) })
}

/// Show commits until the pane closes or a side goes away.
fn show(session: u64, host_link: &mut Link, ch: &mut Link, mut live: Live, device: &Device, shared: &Shared, wake: RawFd) -> Result<(), End> {
    let mut releases = Vec::new();
    let mut sending = Sending::default();
    loop {
        while let Some(f) = ch.ready()? {
            let Live { textures, commits } = &mut live;
            match commits.take(&f, |i| textures[i].generation()) {
                Verdict::Show { buffer, seq, scrolled } => {
                    let b = commits.surface().buffers[buffer as usize];
                    shared.show(seq, textures[buffer as usize].clone(), (b.width, b.height), scrolled);
                    sending.shown(session);
                }
                Verdict::Pass => page_said(&f, shared, session),
                Verdict::Refused { why, close } => {
                    crate::lx_warn!("web_pane.commit_refused", why, session = session);
                    if close {
                        return Err(failed("the content process kept sending bad commits", why));
                    }
                }
            }
        }
        let [from_ch, from_host, woken] = wait([ch.s.as_raw_fd(), host_link.s.as_raw_fd(), wake])?;
        if woken {
            drain(wake)?;
            sending.send(ch, &mut live, device, shared)?;
            live.commits.release(shared.drawn(), &mut releases);
            for seq in releases.drain(..) {
                ch.send(&display::released(seq), &[])?;
            }
        }
        if from_host {
            // nothing the host says is acted on yet; reading keeps a
            // hang-up from going unseen
            host_link.fill()?;
            while host_link.ready()?.is_some() {}
            host_link.fds.clear();
        }
        if from_ch {
            ch.fill()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_buffer_made_here_passes_the_surface_header_check() {
        let (fd, m, b) = gpu::make_buffer(333, 77).expect("shared memory");
        let s = Surface { epoch: 1, scale: 100, buffers: vec![b] };
        assert_eq!(s.check_header(0, m.bytes(), m.len() as u64), Ok(()));
        assert_eq!(os::size_of(fd.as_fd()).unwrap(), b.bytes);
        assert_eq!(b.bytes % gpu::page_size(), 0);
        // the frame round-trips with its one descriptor
        assert_eq!(Surface::from_frame(&s.frame(), 1), Ok(s));
    }

    #[test]
    fn web_pane_linear_texture_alignment_checks_offset_and_stride() {
        let (_, _, b) = gpu::make_buffer(333, 77).expect("shared memory");
        let header = madcawl_wire::shm::HEADER_LEN;
        assert!(gpu::linear_ok(16, header, b.stride as usize));
        assert!(!gpu::linear_ok(256, header, 1024), "the 64-byte pixel offset misses 256");
        assert!(!gpu::linear_ok(16, 64, 1336), "a stride off the alignment");
        assert!(!gpu::linear_ok(0, 64, 1344));
    }
}
