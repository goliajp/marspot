//! Keeping a pane's view offset where its session publishes.
//!
//! The session moves its own view: when the pane is scrolled back and
//! the program prints, the session raises the offset by the rows that
//! went into history so the same lines stay in view.  The mirror here
//! holds no history and never sees those rows go, so the pane takes the
//! offset from the frames instead, but only from a frame that answers
//! every scroll and prompt jump the pane has asked for.  While an ask is
//! in flight, frames published before the session applied it still
//! carry the old offset, and taking that would undo the person's scroll.

use super::{L3Conn, Pane, PaneBackend};
use crate::grid_shm::GridSnapshot;

/// The pane's last view ask and what the last frame said about it.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ViewSync {
    /// The number of the last ask sent; `None` before the first.
    asked: Option<u16>,
    /// The last frame's offset and the number of the last ask the
    /// session had applied when it published it.
    frame: (u16, u16),
}

impl ViewSync {
    /// An ask numbered `seq` went to the session.
    pub(super) fn asked(&mut self, seq: u16) {
        self.asked = Some(seq);
    }

    /// The offset the last frame was published at, if that frame
    /// answers every ask sent.  A session that does not number its
    /// frames never answers one, and the pane keeps its own offset.
    pub(super) fn settled(&self) -> Option<u16> {
        let (off, seq) = self.frame;
        self.asked.is_none_or(|a| a == seq).then_some(off)
    }
}

impl L3Conn {
    /// Note what the frame just read says about the view; when it
    /// answers every ask, the session is at its offset, so a scroll
    /// there is not sent.  Before this core's first ask, the count goes
    /// on from the number the session last applied: the session keeps
    /// it from the core before this one, and counting again from 0
    /// would let a later ask carry that same number, so a frame from
    /// before that ask would be taken as its answer.
    pub(super) fn took_frame(&mut self, snap: &GridSnapshot) {
        self.view_sync.frame = (snap.view_offset, snap.view_seq);
        if self.view_sync.asked.is_none() {
            self.view_seq = snap.view_seq as u32;
        }
        if let Some(off) = self.view_sync.settled() {
            self.req_view_offset = Some(off);
        }
    }
}

impl PaneBackend {
    /// Where an L3 session's view is, when its last frame answers every
    /// ask; `None` for other backends and while an ask is in flight.
    pub fn l3_settled_view(&self) -> Option<u16> {
        match self {
            PaneBackend::L3(c) => c.view_sync.settled(),
            _ => None,
        }
    }
}

impl Pane {
    /// Drain the PTY reader into the terminal parser, or read an L3
    /// session's newest frame and follow its view. Returns the number
    /// of bytes consumed (1 for a frame); `0` means the wake was
    /// spurious or the channel went empty. Containers use this in the
    /// `user_event` callback to decide whether to request_redraw.
    pub fn pump(&mut self) -> usize {
        let n = self.session.pump();
        if n > 0
            && let Some(off) = self.session.l3_settled_view()
        {
            self.view_offset = off;
        }
        n
    }
}

#[cfg(test)]
#[path = "view_sync_tests.rs"]
mod tests;
