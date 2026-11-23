//! Conditions the shell waits for on this pane's output (`PaneWatch`).
//!
//! A watch is looked at when it is registered, after each batch of
//! output, after the shell's process group changed, and when a watch
//! that becomes true with time (`quiet`, `prompt`) is due.  One that
//! came true is reported with `PaneWatchFired` and kept, reported,
//! until the shell cancels it: a report the writer dropped is sent
//! again on the next turn.

use std::time::{Duration, Instant};

use marspot_term::shell_proto::{
    DoneCode, Frame, MsgType, PaneWatchFired, WatchBody, WatchKind, WatchReason, encode_pane_watch_fired,
};

use crate::SessionImpl;
use crate::control_writer::ControlWriter;
use crate::pane_ctl::{local, local_mut, shell_in_front};
use crate::track::{At, Batch, Track};
use crate::typing::Reserved;
use crate::watch_text::{BATCH_ROWS, Needle, stream_text};

mod front;
mod scan;

use front::Front;
use scan::{Scan, above_bottom, text_line};

/// Slots per session.  Slot 7 is kept for watch ids with the top bit
/// set, which the shell gives the watches of its helpers plugin; every
/// other id gets slots 0 to 6.  A caller that fills its seven cannot
/// keep a helper from watching its own pane, and a helper never takes a
/// caller's slot.
const SLOTS: usize = 8;
const HELPER_SLOT: usize = SLOTS - 1;
const HELPER_ID: u32 = 1 << 31;
const MAX_TEXT: usize = 256;
const QUIET_MS: std::ops::RangeInclusive<u32> = 100..=600_000;
/// How long the shell's prompt must be quiet before `prompt` holds.
const PROMPT_QUIET: Duration = Duration::from_millis(150);
/// How long it must be quiet instead while the input start mark the
/// shell set before has not come: a shell that stopped setting it
/// (`exec bash`) is waited for this long once, not until the caller's
/// deadline.
const PROMPT_UNMARKED_QUIET: Duration = Duration::from_secs(1);
/// When to try again a report the writer refused.
const RETRY: Duration = Duration::from_millis(10);
/// Least time between two logs of batches checked short.
const CUT_LOG_EVERY: Duration = Duration::from_secs(60);

type Code = (DoneCode, &'static str);
const OK: Code = (DoneCode::Ok, "");

enum State {
    /// Reserved by a `PaneType` that carries it, armed when the input
    /// is written.  Nothing looks at it until then.
    Pending,
    Waiting {
        /// `prompt` held but for the foreground: no deadline until the
        /// output or the process group changes.
        parked: bool,
    },
    Fired {
        report: PaneWatchFired,
        sent: bool,
    },
}

struct Slot {
    body: WatchBody,
    state: State,
    /// A text watch's text, decoded.
    needle: Option<Box<Needle>>,
    /// Where the output after `since` started.
    from: At,
    /// For a `prompt` watch.
    front: Front,
}

pub(crate) struct Watches {
    sid: u64,
    slots: [Option<Slot>; SLOTS],
    /// A report was refused by a full writer queue.
    retry: bool,
    /// Reports to treat as refused, for a test of the retry.
    refuse: u32,
    /// A waiting text watch's text is all spaces: check every batch.
    every_batch: bool,
    /// Batches checked short since the last log, and when that was.
    cut: (u32, Option<Instant>),
}

impl Watches {
    pub(crate) fn new(sid: u64) -> Self {
        let refuse = std::env::var("MARSPOT_TEST_REFUSE_WATCH_REPORTS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        Self { sid, slots: Default::default(), retry: false, refuse, every_batch: false, cut: (0, None) }
    }

    #[cfg(test)]
    pub(crate) fn check_every_batch(&mut self) {
        self.every_batch = true;
    }

    #[cfg(test)]
    pub(crate) fn met(&self) -> Option<Option<u64>> {
        self.slots.iter().flatten().find_map(|s| match &s.state {
            State::Fired { report, .. } if report.reason == WatchReason::Met => Some(report.line),
            _ => None,
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    /// A `PaneWatch`: register, cancel or clear.
    pub(crate) fn register(&mut self, body: WatchBody, session: &mut SessionImpl, track: &mut Track) -> Code {
        if body.kind == WatchKind::Cancel {
            for slot in &mut self.slots {
                if body.watch_id == 0 || slot.as_ref().is_some_and(|s| s.body.watch_id == body.watch_id) {
                    *slot = None;
                }
            }
            self.stream_texts(session);
            return OK;
        }
        match self.reserve(body, session) {
            Ok((at, _)) => {
                self.arm(at, track, session);
                OK
            }
            Err(code) => code,
        }
    }

    /// Take a slot for `body`, replacing a watch with its id; the
    /// refusal if there is none.  It is armed by [`Self::activate`].
    pub(crate) fn reserve(&mut self, body: WatchBody, session: &mut SessionImpl) -> Result<Reserved, Code> {
        if let Some(bad) = invalid(&body) {
            return Err(bad);
        }
        let Some(at) = self.slot_for(body.watch_id) else {
            return Err((DoneCode::WatchTableFull, "watch table full"));
        };
        let id = body.watch_id;
        let needle = if body.kind == WatchKind::Text { Needle::new(&body.text) } else { None };
        self.slots[at] = Some(Slot { body, state: State::Pending, needle, from: (0, 0), front: Front::default() });
        // the watch it replaced may have been a text one
        self.stream_texts(session);
        Ok((at, id))
    }

    /// Arm a reserved watch from `since`; nothing if it was cancelled
    /// or replaced meanwhile.
    pub(crate) fn activate(&mut self, (at, id): Reserved, since: u64, session: &mut SessionImpl, track: &mut Track) {
        if let Some(slot) = self.pending(at, id) {
            slot.body.since_pos = since;
            self.arm(at, track, session);
        }
    }

    /// Free a reserved watch whose input was not written.
    pub(crate) fn release(&mut self, (at, id): Reserved) {
        if self.pending(at, id).is_some() {
            self.slots[at] = None;
        }
    }

    /// Free every reserved watch.
    pub(crate) fn release_all(&mut self) {
        for slot in &mut self.slots {
            if slot.as_ref().is_some_and(|s| matches!(s.state, State::Pending)) {
                *slot = None;
            }
        }
    }

    fn pending(&mut self, at: usize, id: u32) -> Option<&mut Slot> {
        self.slots[at].as_mut().filter(|s| s.body.watch_id == id && matches!(s.state, State::Pending))
    }

    fn arm(&mut self, at: usize, track: &mut Track, session: &mut SessionImpl) {
        let Some(slot) = self.slots[at].as_mut() else { return };
        slot.from = track.at(slot.body.since_pos);
        slot.state = State::Waiting { parked: false };
        if slot.body.kind == WatchKind::Prompt {
            slot.front.seen(shell_in_front(session), Instant::now());
        }
        self.stream_texts(session);
        self.look(at, session, track, Scan::Register);
    }

    fn slot_for(&self, id: u32) -> Option<usize> {
        if let Some(at) = self.slots.iter().position(|s| s.as_ref().is_some_and(|s| s.body.watch_id == id)) {
            return Some(at);
        }
        let range = if id & HELPER_ID != 0 { HELPER_SLOT..SLOTS } else { 0..HELPER_SLOT };
        range.into_iter().find(|&i| self.slots[i].is_none())
    }

    /// Look at every waiting watch after a turn.
    pub(crate) fn evaluate(&mut self, session: &mut SessionImpl, track: &mut Track, batch: Option<Batch>, procs: bool) {
        let mut texts = false;
        for slot in self.slots.iter_mut().flatten() {
            if let State::Waiting { parked } = &mut slot.state {
                *parked &= !(procs || batch.is_some());
                texts |= slot.body.kind == WatchKind::Text;
                if procs && slot.body.kind == WatchKind::Prompt {
                    slot.front.seen(shell_in_front(session), Instant::now());
                }
            }
        }
        // a held grid has not got the batch yet: whether to check is
        // asked again once the hold ends and the grid changes
        let mut whole = 0u8;
        let check = texts && batch.is_some() && !local(session).is_holding() && {
            let (stream, held) = local_mut(session).take_text_check();
            whole = held;
            stream || self.every_batch || above_bottom(session)
        };
        for at in 0..SLOTS {
            let scan = match batch {
                Some(batch) if check => Scan::Batch { batch, whole: whole & 1 << at != 0 },
                _ => Scan::None,
            };
            self.look(at, session, track, scan);
        }
        self.log_cut(session.id());
    }

    fn look(&mut self, at: usize, session: &mut SessionImpl, track: &mut Track, scan: Scan) {
        let Some(slot) = self.slots[at].as_mut() else { return };
        let State::Waiting { parked } = &mut slot.state else { return };
        let b = &slot.body;
        let now = Instant::now();
        let after = track.pos() > b.since_pos;
        let last_output = track.last_output();
        let quiet_for = |from: Instant, d: Duration| after && now >= from + d;
        let met = match b.kind {
            WatchKind::Change => after.then_some((track.pos(), None, None)),
            WatchKind::CommandEnd => match track.command_end() {
                Some((pos, status)) if pos > b.since_pos => Some((pos, status.map(i16::from), None)),
                _ => None,
            },
            WatchKind::Quiet => quiet_for(last_output, Duration::from_millis(b.quiet_ms.into())).then_some((track.pos(), None, None)),
            WatchKind::Prompt if !*parked && quiet_for(slot.front.quiet_from(last_output), prompt_quiet(track, b.since_pos)) => {
                *parked = !shell_in_front(session);
                if *parked {
                    slot.front.seen(false, now);
                }
                let met = !*parked && !slot.front.back_unseen(now);
                if met && !track.prompt_drawn(b.since_pos) {
                    track.forget_input_start();
                }
                met.then_some((track.pos(), None, None))
            }
            WatchKind::Text if after => {
                let line = slot.needle.as_deref().and_then(|n| text_line(session, n, (b.since_pos, slot.from), scan, &mut self.cut.0));
                line.map(|l| (track.pos(), None, Some(l)))
            }
            _ => None,
        };
        if let Some((pos, status, line)) = met {
            let report = PaneWatchFired { sid: self.sid, watch_id: b.watch_id, reason: WatchReason::Met, pos, status, line };
            let text = b.kind == WatchKind::Text;
            slot.state = State::Fired { report, sent: false };
            if text {
                self.stream_texts(session);
            }
        }
    }

    /// Set the texts the session reads its output for: one per waiting
    /// text watch.
    fn stream_texts(&mut self, session: &mut SessionImpl) {
        let mut set: [(usize, &[u8]); SLOTS] = [(0, &[]); SLOTS];
        let mut n = 0;
        self.every_batch = false;
        for (at, slot) in self.slots.iter().enumerate() {
            let Some(slot) = slot else { continue };
            if slot.body.kind == WatchKind::Text && matches!(slot.state, State::Waiting { .. }) {
                match stream_text(&slot.body.text) {
                    Some(t) => {
                        set[n] = (at, t);
                        n += 1;
                    }
                    None => self.every_batch = true,
                }
            }
        }
        local_mut(session).look_for_texts(&set[..n]);
    }

    fn log_cut(&mut self, sid: u64) {
        let (count, last) = self.cut;
        if count == 0 || last.is_some_and(|t| t.elapsed() < CUT_LOG_EVERY) {
            return;
        }
        marspot_term::lx_warn!(
            "l3.watch.batch_cut",
            "a batch scrolled past more rows than a text watch reads; only the newest were checked",
            session_id = sid,
            batches = count,
            rows_read = BATCH_ROWS
        );
        self.cut = (0, Some(Instant::now()));
    }

    /// When a watch that becomes true with time is due, or a refused
    /// report is to be tried again.
    pub(crate) fn due_in(&self, track: &Track, now: Instant) -> Option<Duration> {
        let mut due = self.retry.then_some(RETRY);
        for slot in self.slots.iter().flatten() {
            let State::Waiting { parked } = slot.state else { continue };
            let at = match slot.body.kind {
                WatchKind::Quiet => track.last_output() + Duration::from_millis(slot.body.quiet_ms.into()),
                WatchKind::Prompt if !parked => slot.front.quiet_from(track.last_output()) + prompt_quiet(track, slot.body.since_pos),
                _ => continue,
            };
            if track.pos() > slot.body.since_pos {
                let left = at.saturating_duration_since(now);
                due = Some(due.map_or(left, |d| d.min(left)));
            }
        }
        due
    }

    /// Every waiting watch ends: the image is being replaced
    /// (`Restarting`), or the shell exited (`SessionEnded`, which is
    /// what an `exit` watch waits for).
    pub(crate) fn end_all(&mut self, reason: WatchReason, track: &Track) {
        for slot in self.slots.iter_mut().flatten() {
            if matches!(slot.state, State::Waiting { .. }) {
                let met = reason == WatchReason::SessionEnded && slot.body.kind == WatchKind::Exit;
                let reason = if met { WatchReason::Met } else { reason };
                let report = PaneWatchFired { sid: self.sid, watch_id: slot.body.watch_id, reason, pos: track.pos(), status: None, line: None };
                slot.state = State::Fired { report, sent: false };
            }
        }
    }

    /// A report has not gone out yet.
    pub(crate) fn unsent(&self) -> bool {
        self.slots.iter().flatten().any(|s| matches!(s.state, State::Fired { sent: false, .. }))
    }

    /// Send the reports not sent yet, in slot order, until the writer
    /// refuses one.
    pub(crate) fn send_fired(&mut self, mut poke: Option<&mut ControlWriter>) {
        self.retry = false;
        for slot in self.slots.iter_mut().flatten() {
            let State::Fired { report, sent: sent @ false } = &mut slot.state else { continue };
            let Some(w) = poke.as_deref_mut() else { return };
            let frame = Frame::new(MsgType::PaneWatchFired, encode_pane_watch_fired(report));
            let refused = if self.refuse > 0 {
                self.refuse -= 1;
                true
            } else {
                !w.send(frame)
            };
            if refused {
                self.retry = true;
                return;
            }
            *sent = true;
        }
    }
}

/// How long the output after `since` must be quiet, the shell in front,
/// before a `prompt` watch holds.
fn prompt_quiet(track: &Track, since: u64) -> Duration {
    if track.prompt_drawn(since) { PROMPT_QUIET } else { PROMPT_UNMARKED_QUIET }
}

fn invalid(b: &WatchBody) -> Option<Code> {
    match b.kind {
        WatchKind::Text if b.text.is_empty() => Some((DoneCode::Other, "empty text")),
        WatchKind::Text if b.text.len() > MAX_TEXT => Some((DoneCode::Other, "text over 256 bytes")),
        WatchKind::Quiet if !QUIET_MS.contains(&b.quiet_ms) => Some((DoneCode::Other, "quiet_ms not in 100..=600000")),
        WatchKind::Unknown(_) | WatchKind::Cancel => Some((DoneCode::Other, "unknown watch kind")),
        _ => None,
    }
}
