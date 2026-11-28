//! What a web pane makes of the frames it is sent: the version answers
//! of both hellos, and which commits of the content process it shows
//! and which buffers it hands back.  No I/O here; the other side of
//! the display channel is untrusted, so every commit is checked before
//! its pixels are drawn.

use madcawl_wire::display::{Commit, CommitError, Surface, MAX_BUFFERS};
use madcawl_wire::shm::Header;
use madcawl_wire::{host, Frame, Hello, Outcome, Range};

/// Bad commits in a row before the channel is closed.
pub const BAD_IN_A_ROW: u32 = 3;

/// Why the other side of a hello is not spoken to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unspoken {
    /// Its newest version is older than ours; `None` when it answered
    /// with a refusal, as a host from before the hello does.
    TooOld(Option<Range>),
    TooNew(Range),
    /// Not a hello at all, or one with bad fields.
    Bad(String),
}

/// The version agreed from the answer `f` to a hello of `ours`.
pub fn hello_answer(f: &Frame, kind: u16, ours: Range) -> Result<u32, Unspoken> {
    if host::refusal(f).is_some() {
        return Err(Unspoken::TooOld(None));
    }
    if f.kind != kind {
        return Err(Unspoken::Bad(format!("a frame of kind {} for the hello", f.kind)));
    }
    let theirs = Hello::from_frame(f).map_err(|e| Unspoken::Bad(e.to_string()))?;
    match ours.negotiate(theirs.range) {
        // the answer must name the version, and it must be one we speak
        Outcome::Agreed(v) if theirs.version == Some(v) => Ok(v),
        Outcome::Agreed(_) => Err(Unspoken::Bad(format!("the answer names version {:?}", theirs.version))),
        Outcome::PeerTooOld(r) => Err(Unspoken::TooOld(Some(r))),
        Outcome::PeerTooNew(r) => Err(Unspoken::TooNew(r)),
    }
}

/// What to do with one frame from the content process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Show buffer `buffer`, the pixels of commit `seq`.
    Show { buffer: u32, seq: u64 },
    /// Not shown and not held against the peer: a commit drawn into a
    /// surface sent before this one, or a kind this build does not know.
    Pass,
    /// Not shown, and counted; `close` once there were too many.
    Refused { why: &'static str, close: bool },
}

/// The display end's side of one surface: the last commit taken, the
/// buffer drawn now and which commit holds each buffer until released.
pub struct Commits {
    surface: Surface,
    last: u64,
    showing: Option<u32>,
    held: [Option<u64>; MAX_BUFFERS],
    bad: u32,
}

impl Commits {
    pub fn new(surface: Surface) -> Commits {
        Commits { surface, last: 0, showing: None, held: [None; MAX_BUFFERS], bad: 0 }
    }

    pub fn surface(&self) -> &Surface {
        &self.surface
    }

    /// Judge `f`; `generation(i)` reads buffer `i`'s generation field,
    /// which the content process sets to the commit's sequence once the
    /// pixels are written.
    pub fn take(&mut self, f: &Frame, generation: impl Fn(usize) -> u64) -> Verdict {
        if f.kind != madcawl_wire::display::kind::COMMIT {
            return Verdict::Pass;
        }
        let checked = Commit::from_frame(f)
            .map_err(|_| "malformed")
            .and_then(|c| match c.check(&self.surface, self.last, self.showing) {
                Err(CommitError::Epoch) => Ok(None),
                Err(e) => Err(why(e)),
                Ok(()) if generation(c.buffer as usize) != c.seq => Err("pixels not marked written"),
                Ok(()) => Ok(Some(c)),
            });
        match checked {
            Ok(None) => Verdict::Pass,
            Ok(Some(c)) => {
                self.bad = 0;
                self.last = c.seq;
                self.showing = Some(c.buffer);
                self.held[c.buffer as usize] = Some(c.seq);
                Verdict::Show { buffer: c.buffer, seq: c.seq }
            }
            Err(why) => {
                self.bad += 1;
                Verdict::Refused { why, close: self.bad >= BAD_IN_A_ROW }
            }
        }
    }

    /// The commits whose buffers can go back now that a frame showing
    /// commit `drawn` has finished on the GPU: every one held but the
    /// one shown, once the one shown has been drawn.
    pub fn release(&mut self, drawn: u64, out: &mut Vec<u64>) {
        let Some(showing) = self.showing else { return };
        if self.held[showing as usize].is_none_or(|s| drawn < s) {
            return;
        }
        for (i, h) in self.held.iter_mut().enumerate() {
            if i as u32 != showing
                && let Some(seq) = h.take()
            {
                out.push(seq);
            }
        }
    }
}

fn why(e: CommitError) -> &'static str {
    match e {
        CommitError::Epoch => "an earlier surface",
        CommitError::Seq => "sequence not after the last",
        CommitError::NoBuffer => "no such buffer",
        CommitError::Busy => "the buffer being shown",
        CommitError::Dirty => "dirty rectangle outside the buffer",
        CommitError::Doc => "document size out of range",
    }
}

/// A pane's page size in CSS px and its buffers' in device pixels, for
/// a pane `w` by `h` device pixels at `scale` hundredths: the page as
/// large as fits, the buffers exactly its size.
pub fn page_size(w: f64, h: f64, scale: u32) -> Option<((u32, u32), (u32, u32))> {
    let s = f64::from(scale) / 100.0;
    let css = |px: f64| (px / s).floor();
    let (cw, ch) = (css(w), css(h));
    if !(cw >= 1.0 && ch >= 1.0) {
        return None;
    }
    let px = |c: f64| (c * s).floor() as u32;
    let (pw, ph) = (px(cw), px(ch));
    let max = madcawl_wire::MAX_SIDE;
    if pw == 0 || ph == 0 || pw > max || ph > max {
        return None;
    }
    Some(((cw as u32, ch as u32), (pw, ph)))
}

/// The bytes of one buffer of `w` by `h` pixels: header and rows,
/// rounded up to whole pages, as wrapping it in a GPU buffer needs.
pub fn buffer_bytes(w: u32, h: u32, page: u64) -> u64 {
    Header::needed(w, h).div_ceil(page) * page
}

#[cfg(test)]
#[path = "frames_tests.rs"]
mod tests;
