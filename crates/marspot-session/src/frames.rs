//! One turn of the loop's work on the pane's output: parse what the
//! reader has queued, keep a view that is scrolled back where it was,
//! and put a frame of the grid out when one is due.

use std::time::Duration;

use marspot_term::grid_shm::{
    FLAG_ALT_SCREEN, FLAG_ALT_SCROLL, FLAG_APP_CURSOR_KEYS, FLAG_BRACKETED_PASTE,
    FLAG_CURSOR_VISIBLE, FLAG_MOUSE_ANY_MOTION, FLAG_MOUSE_DRAG, FLAG_MOUSE_SGR,
    FLAG_MOUSE_TRACKING, GridShmWriter,
};
use marspot_term::loop_watch::LoopWatch;
use marspot_term::lx_event;
use marspot_term::shell_proto::{Frame, MsgType};

use crate::SessionImpl;
use crate::control_writer::ControlWriter;
use crate::local_session::Pace;
use crate::publish_gate::{BlankGate, FloodGate, SyncGate, grid_is_blank};

/// Most one iteration spends parsing before it goes round again to
/// publish and apply a resize. A frame at 120 Hz.
const PUMP_BUDGET: Duration = Duration::from_millis(8);

/// The region the grid is published into and what decides when.
pub(crate) struct Frames {
    shm: GridShmWriter,
    sync_gate: SyncGate,
    blank_gate: BlankGate,
    flood_gate: FloodGate,
    /// Scrollback view offset L2 last asked us to publish (0 = live tail).
    view_offset: u16,
    /// The grid's scroll-push counter at the end of the last turn.
    last_scroll_push: u64,
    /// The last turn left chunks queued when its budget ran out.
    output_queued: bool,
}

/// What a turn did before its output that shows on screen by itself.
pub(crate) struct Shown {
    /// A local echo painted ahead of the PTY's.
    pub(crate) predicted: bool,
    pub(crate) resized: bool,
    pub(crate) modes_changed: bool,
    /// The view offset L2 asked for this turn.
    pub(crate) scroll: Option<u16>,
}

pub(crate) enum Turn {
    /// How many bytes were parsed.
    Parsed(usize),
    /// The pane's program is gone and its last output is on screen.
    Exited,
}

impl Frames {
    pub(crate) fn new(shm: GridShmWriter) -> Self {
        Self {
            shm,
            sync_gate: SyncGate::default(),
            blank_gate: BlankGate::default(),
            flood_gate: FloodGate::default(),
            view_offset: 0,
            last_scroll_push: 0,
            output_queued: false,
        }
    }

    pub(crate) fn shm(&mut self) -> &mut GridShmWriter {
        &mut self.shm
    }

    /// Track the grid's scroll-push counter so we can auto-pin
    /// `view_offset` when the live grid scrolls a line into scrollback
    /// while the user is viewing scrollback (`view_offset > 0`).  Without
    /// this, every PTY line the shell emits while the user is scrolled
    /// back shifts the visible content downward by one row — the
    /// "老内容被新内容覆盖" symptom.  L2's `pump_all` runs the symmetric
    /// bump so the two sides stay in lockstep without an extra
    /// forward_scroll round-trip.
    pub(crate) fn follow_scroll_from(&mut self, session: &SessionImpl) {
        self.last_scroll_push = session.terminal().grid().scroll_push_count();
    }

    /// How long until a gate that is holding a frame has to be looked
    /// at again, or the turn after a flood is due.
    ///
    /// A gate that is holding a frame owes a flush even if the PTY
    /// has gone quiet: without this, a program that wipes the
    /// screen and then says nothing would leave the pane showing
    /// the content it wiped, forever.  Both gates release inside
    /// their own cap, so waking a little sooner than the shorter
    /// one is enough.
    pub(crate) fn due_in(&self) -> Option<Duration> {
        let owed = self.sync_gate.owes_a_frame() || self.blank_gate.owes_a_frame();
        [owed.then_some(Duration::from_millis(10)), self.flood_gate.due_in()].into_iter().flatten().min()
    }

    /// The reader's wakes for what is still queued were taken by the
    /// last turn's drain.
    pub(crate) fn output_queued(&self) -> bool {
        self.output_queued
    }

    /// A key is on its way to the pane's program.
    pub(crate) fn typed(&mut self, session: &mut SessionImpl) {
        self.flood_gate.typed();
        session.typed();
    }

    /// How far one turn parses.  A key that arrives ends it within a
    /// few kilobytes; and while a key's answer is awaited it ends after
    /// each chunk, or slice, so the frame that shows the answer is not
    /// kept waiting for what is behind it.
    fn pace(&self) -> Pace<'static> {
        Pace {
            budget: PUMP_BUDGET,
            follow: self.flood_gate.follows(),
            key_waiting: crate::input::waiting(),
        }
    }

    /// Publish the grid and, if connected to L2, poke it.
    pub(crate) fn publish(&mut self, session: &mut SessionImpl, poke: Option<&mut ControlWriter>) {
        publish_and_poke(&mut self.shm, session, self.view_offset, &mut self.sync_gate, &mut self.blank_gate, poke);
    }

    /// Final pump before post-loop dispatcher decides
    /// execv-handoff vs clean-exit.  Bounded: the reader is
    /// still running, and a program that floods faster than the
    /// parser reads would keep an unbounded one here for as long
    /// as it floods.  What is still queued is parsed once the
    /// reader has been stopped.
    pub(crate) fn last_turn(&mut self, session: &mut SessionImpl, poke: Option<&mut ControlWriter>, watch: &mut LoopWatch) {
        watch.phase("resize-pump");
        let _ = session.pump_within(self.pace());
        watch.phase("resize-publish");
        self.publish(session, poke);
    }

    /// Parse within the budget and publish what there is to show.
    pub(crate) fn turn(
        &mut self,
        session: &mut SessionImpl,
        mut poke: Option<&mut ControlWriter>,
        watch: &mut LoopWatch,
        shown: Shown,
    ) -> Turn {
        let Shown { predicted, resized, modes_changed, scroll } = shown;
        // A scroll changes which window we publish even with no new output.
        let scrolled = match scroll {
            Some(off) if off != self.view_offset => {
                self.view_offset = off;
                true
            }
            _ => false,
        };

        watch.phase("pump");
        let (n, queued) = session.pump_within(self.pace());
        self.output_queued = queued;
        // Auto-pin view_offset on scrollback push: when the grid scrolls
        // a row into scrollback while the user is viewing history,
        // shift view_offset by the same delta so the visible window
        // tracks the same absolute content instead of sliding down.
        // L2 runs the symmetric bump in `pump_all` so the two stay in
        // sync without an extra forward_scroll round-trip.
        let cur_scroll_push = session.terminal().grid().scroll_push_count();
        if self.view_offset > 0 && cur_scroll_push > self.last_scroll_push {
            let delta = (cur_scroll_push - self.last_scroll_push).min(u16::MAX as u64) as u16;
            let max = session.terminal().grid().scrollback_len() as u16;
            // F1+8 — `.min(max)` would clamp view_offset DOWN when
            // the L3 just lost scrollback context (e.g. alt-screen
            // switch for a TUI like claudecode whose own scrollback
            // is shorter than the saved-main history we were
            // viewing).  After every PTY chunk that fired a
            // scroll_push, view_offset would snap to ~0 — looks
            // like "Enter 跳完马上弹回".  Skip the bump when max
            // < view_offset; keep the user's intentional position.
            if max >= self.view_offset {
                self.view_offset = self.view_offset.saturating_add(delta).min(max);
            } else {
                lx_event!(
                    "L3_BUMP_SKIP",
                    "skipping view_offset bump (max < view_offset)",
                    session_id = session.id(),
                    view_offset = self.view_offset as u32,
                    max_now = max as u32,
                    delta = delta as u32
                );
            }
        }
        self.last_scroll_push = cur_scroll_push;
        if session.is_exited() {
            session.pump();
            self.publish(session, poke.as_deref_mut());
            lx_event!("SESSION_EXITED", "shelld session exited; exiting cleanly");
            return Turn::Exited;
        }
        // Lines that scrolled off go to their files when a burst ends,
        // and a flood's last ones on the turn after it.
        let flood = n >= FloodGate::BULK_BYTES || queued;
        if !flood {
            session.terminal().grid().scrollback_hand_off();
        }
        self.flood_gate.turn(flood);
        // Republish on PTY output, a local echo that painted ahead of it,
        // a resize (grid shape changed), or a scroll (window changed) —
        // even when no bytes pumped this tick.
        if n > 0
            || predicted
            || resized
            || scrolled
            || modes_changed
            // A frame is being withheld; this tick is what discharges
            // it once its gate's cap has run out.
            || self.sync_gate.owes_a_frame()
            || self.blank_gate.owes_a_frame()
            || self.flood_gate.owes_a_frame()
        {
            if scrolled {
                lx_event!(
                    "L3_PUBLISH",
                    "publish_and_poke after scroll",
                    session_id = session.id(),
                    view_offset = self.view_offset as u32,
                    cause = "scrolled"
                );
            }
            watch.phase("publish");
            let bulk = flood && !(predicted || resized || scrolled || modes_changed);
            if !self.flood_gate.hold(bulk) {
                self.publish(session, poke);
            }
        }

        Turn::Parsed(n)
    }
}

/// Publish the session's grid window at `view_offset` (rows up from the
/// live tail, clamped to the available scrollback) + cursor/mode flags
/// into the shared framebuffer for L2 to render.  Returns `true` when
/// the publish actually wrote a new snapshot (i.e. the grid+cursor+
/// flags differ from the previous publish).  The caller uses the bool
/// to decide whether to send a GridReady poke — skipping the poke on
/// a content-identical publish is what keeps idle L2 asleep when a TUI
/// pumps the PTY at its own redraw cadence without moving any cell.
fn publish(shm: &mut GridShmWriter, session: &SessionImpl, view_offset: u16) -> bool {
    let term = session.terminal();
    let mut flags = 0u32;
    if term.cursor_visible() {
        flags |= FLAG_CURSOR_VISIBLE;
    }
    if term.cursor_key_application_mode() {
        flags |= FLAG_APP_CURSOR_KEYS;
    }
    if term.bracketed_paste_mode() {
        flags |= FLAG_BRACKETED_PASTE;
    }
    if term.mouse_tracking_mode() != marspot_term::terminal::MouseTrackingMode::Off {
        flags |= FLAG_MOUSE_TRACKING;
    }
    if term.mouse_sgr_encoding() {
        flags |= FLAG_MOUSE_SGR;
    }
    match term.mouse_tracking_mode() {
        marspot_term::terminal::MouseTrackingMode::ButtonEvent => flags |= FLAG_MOUSE_DRAG,
        marspot_term::terminal::MouseTrackingMode::AnyEvent => {
            flags |= FLAG_MOUSE_DRAG | FLAG_MOUSE_ANY_MOTION
        }
        _ => {}
    }
    if term.in_alt_screen() {
        flags |= FLAG_ALT_SCREEN;
    }
    // The program's own statement that the wheel is the arrow keys
    // here — see `Terminal::alt_scroll`.  Published so L2 routes on
    // what it was told, not on what it can infer from the picture.
    if term.alt_scroll_mode() {
        flags |= FLAG_ALT_SCROLL;
    }
    // Clamp here too: L2 clamps against the scrollback_len it last saw, but
    // scrollback can shrink (alt-screen enter / reset) between L2's request
    // and this publish, so never hand the writer an out-of-range offset.
    let off = view_offset.min(term.grid().scrollback_len() as u16);
    shm.publish_title(term.osc_title());
    shm.publish_if_changed(term.grid(), off, flags)
}

/// Publish the grid and, if connected to L2, poke it so it re-reads the
/// shm — keeps L2 event-driven instead of polling per frame.
fn publish_and_poke(
    shm: &mut GridShmWriter,
    session: &mut SessionImpl,
    view_offset: u16,
    gate: &mut SyncGate,
    blank_gate: &mut BlankGate,
    mut poke: Option<&mut ControlWriter>,
) {
    // A clipboard write goes out before any of the gates below.  It
    // is not a frame and it is not about the grid: a program that
    // yanked something should not have to wait for a redraw, and the
    // sync-output gate can withhold one indefinitely.
    if let Some(text) = session.terminal_mut().take_osc_clipboard()
        && let Some(w) = poke.as_deref_mut()
    {
        let _ = w.send(Frame::new(MsgType::PaneClipboardWrite, text.into_bytes()));
    }

    // Withheld frames are not lost: the bytes are already in the grid,
    // and the publish that follows the closing `l` carries all of them
    // at once — which is the whole point of the mode.
    if gate.hold(session.terminal().sync_output_active()) {
        return;
    }
    if blank_gate.hold(grid_is_blank(session.terminal().grid())) {
        return;
    }
    gate.discharged();
    blank_gate.discharged();
    let changed = publish(shm, session, view_offset);
    // F3+3.6 — OSC 7 push-based cwd publish removed; L2 now pull-
    // fetches cwd via `proc_pidinfo` when the user opens LayoutModal.
    let _ = &mut poke;
    // Dedup: a content-identical publish doesn't wake L2.  Without
    // this, busy TUIs (claudecode, htop, vim cursor) emit redraw
    // bytes that produced bit-identical shm snapshots, and each one
    // woke L2 for a full re-render — measured ~30 L3Ready/s at idle
    // before the gate.  When the grid genuinely changed we still
    // poke once, same as before.
    if !changed {
        return;
    }
    if let Some(w) = poke {
        // Best-effort: a dead socket just means L2 went away; the next
        // read EOF tears the reader down and the session keeps running.
        // Queued, never written inline — see `ControlWriter`.
        let _ = w.send(Frame::new(MsgType::GridReady, Vec::new()));
    }
}
