//! What decides whether a frame of the grid goes out now: a program in
//! the middle of drawing one, a screen just wiped, and output arriving
//! faster than a display shows it.

use std::time::{Duration, Instant};

/// Holds frames while a program is drawing one (DEC mode 2026).
///
/// `CSI ? 2026 h` means "I am about to repaint; do not show anyone
/// what is on the way".  Publishing mid-update is what makes a full
/// repaint flash: codex brackets every frame this way, and opening its
/// transcript went black on us while iTerm2, which honours the mode,
/// did not (2026-09-06).
///
/// The terminal cannot make a program close what it opened, so the
/// hold is bounded.  A frame withheld for longer than `MAX_HOLD` goes
/// out anyway: a torn frame is a blemish, a frozen pane is a bug.
#[derive(Default)]
pub(crate) struct SyncGate {
    since: Option<std::time::Instant>,
    /// A frame was withheld and nothing has been published since.
    ///
    /// The loop only publishes when bytes arrive, so without this a
    /// program that draws and then goes quiet would leave its last
    /// frame withheld forever.  Cleared by the publish that discharges
    /// it, so a quiet pane goes back to sleeping — idle CPU is a hard
    /// constraint and a permanently-armed timer would break it.
    owed: bool,
}

impl SyncGate {
    /// Generous next to a repaint (codex writes ~13 KB per frame) and
    /// still under the ~200 ms where a stall stops reading as "fast".
    pub(crate) const MAX_HOLD: std::time::Duration = std::time::Duration::from_millis(150);

    /// Should this frame be withheld?  Call once per publish.
    pub(crate) fn hold(&mut self, active: bool) -> bool {
        self.hold_at(active, std::time::Instant::now())
    }

    pub(crate) fn hold_at(&mut self, active: bool, now: std::time::Instant) -> bool {
        if !active {
            self.since = None;
            return false;
        }
        let held = match self.since {
            None => {
                self.since = Some(now);
                true
            }
            // Kept open too long — show what we have and stop holding
            // for this update; the closing `l` resets us.
            Some(t) => now.duration_since(t) < Self::MAX_HOLD,
        };
        self.owed |= held;
        held
    }

    /// Does a withheld frame still need publishing?
    pub(crate) fn owes_a_frame(&self) -> bool {
        self.owed
    }

    /// A publish went out; nothing is owed until something is held again.
    pub(crate) fn discharged(&mut self) {
        self.owed = false;
    }
}

/// Holds a screen that has just been wiped, in case it is a repaint.
///
/// codex clears the screen OUTSIDE its synchronized batch — measured,
/// the bytes read `… CSI ? 2026 l · CSI J · CSI ? 2026 h · <paint> …`
/// — so the wipe lands in an unprotected moment.  We publish once per
/// PTY read, the two arrive in separate reads, and the empty grid
/// between them reaches the display: opening codex's transcript showed
/// a fully blank screen between the old content and the new one
/// (measured as a `38% → 0% → 44%` fill sequence, and reported as
/// "切入 history 会黑屏").  iTerm2 does not flash because it presents
/// on a display cadence, so a wipe and the repaint a millisecond later
/// land in the same shown frame.
///
/// A COMPLETELY blank screen is almost always in transit — a repaint
/// under way, or a `clear` about to be followed by a prompt.  So it
/// waits briefly: if content arrives it is published instead and the
/// blank frame is never seen, and if the screen really is meant to be
/// empty it goes out `MAX_HOLD` later, which nobody can perceive.
///
/// The test is "not one printable cell", which no screen with content
/// passes, and the scan leaves on the first such cell — so the cost
/// falls on blank screens, which are the rare ones.
#[derive(Default)]
pub(crate) struct BlankGate {
    since: Option<std::time::Instant>,
    /// See `SyncGate::owed`.
    owed: bool,
}

impl BlankGate {
    /// Long enough to cover the gap between a wipe and its repaint,
    /// short enough that a screen genuinely meant to be blank appears
    /// at once as far as anyone can tell.
    pub(crate) const MAX_HOLD: std::time::Duration = std::time::Duration::from_millis(50);

    pub(crate) fn hold(&mut self, blank: bool) -> bool {
        self.hold_at(blank, std::time::Instant::now())
    }

    pub(crate) fn hold_at(&mut self, blank: bool, now: std::time::Instant) -> bool {
        if !blank {
            self.since = None;
            return false;
        }
        let held = match self.since {
            None => {
                self.since = Some(now);
                true
            }
            Some(t) => now.duration_since(t) < Self::MAX_HOLD,
        };
        self.owed |= held;
        held
    }

    /// See `SyncGate::owes_a_frame`.
    pub(crate) fn owes_a_frame(&self) -> bool {
        self.owed
    }

    pub(crate) fn discharged(&mut self) {
        self.owed = false;
    }
}

/// Spaces out the frames of bulk output.
///
/// A pane absorbing a flood parses one full read per turn of the loop,
/// and a frame after each is four thousand frames a second -- a hash
/// and a copy of the whole grid every time, for a display that shows a
/// hundred and twenty.  While output arrives a full read at a time, one
/// frame goes out per [`FloodGate::FRAME`].
///
/// Nothing else waits: a turn that parsed less than a full read is the
/// end of a flood or was never one, and for a while after a key was
/// typed every frame goes out as it is ready, so an echo in the middle
/// of a flood is on screen as soon as it would have been.
#[derive(Default)]
pub(crate) struct FloodGate {
    /// When the last frame went out.
    last: Option<Instant>,
    typed: Option<Instant>,
    /// See `SyncGate::owed`.
    owed: bool,
    /// The loop's last turn parsed a flood: one more turn is owed, on
    /// which what the flood left buffered is handed on.
    flood: bool,
}

impl FloodGate {
    /// One frame of a 120 Hz display.
    pub(crate) const FRAME: Duration = Duration::from_millis(8);

    /// What one turn of the loop parses at least while a flood lasts:
    /// one read buffer.
    pub(crate) const BULK_BYTES: usize = 64 * 1024;

    /// How long after a key its echo may still be on its way.
    const ECHO_WINDOW: Duration = Duration::from_millis(100);

    pub(crate) fn typed(&mut self) {
        self.typed = Some(Instant::now());
        // the reader stops gathering for as long as this gate stops holding
        crate::pty_reader::typed();
    }

    /// Whether the turn of the loop now ending parsed a flood.
    pub(crate) fn turn(&mut self, flood: bool) {
        self.flood = flood;
    }

    /// Should the frame after a turn that parsed `bulk` output wait?
    /// Call once per publish.
    pub(crate) fn hold(&mut self, bulk: bool) -> bool {
        self.hold_at(bulk, Instant::now())
    }

    fn hold_at(&mut self, bulk: bool, now: Instant) -> bool {
        let recent = |t: Option<Instant>, window| t.is_some_and(|t| now.saturating_duration_since(t) < window);
        let held = bulk && recent(self.last, Self::FRAME) && !recent(self.typed, Self::ECHO_WINDOW);
        if !held {
            self.last = Some(now);
        }
        self.owed = held;
        held
    }

    /// See `SyncGate::owes_a_frame`.
    pub(crate) fn owes_a_frame(&self) -> bool {
        self.owed
    }

    /// How long until a withheld frame is due, or the turn after a
    /// flood.
    pub(crate) fn due_in(&self) -> Option<Duration> {
        self.due_in_at(Instant::now())
    }

    fn due_in_at(&self, now: Instant) -> Option<Duration> {
        let last = self.last.filter(|_| self.owed || self.flood)?;
        Some((last + Self::FRAME).saturating_duration_since(now))
    }
}

/// Does the visible grid hold nothing a viewer could see?
pub(crate) fn grid_is_blank(grid: &marspot_term::grid::Grid) -> bool {
    for row in 0..grid.rows() {
        for col in 0..grid.cols() {
            let ch = grid.cell(col, row).ch;
            if ch != ' ' && ch != '\0' {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod sync_gate_tests {
    use super::SyncGate;
    use std::time::{Duration, Instant};

    /// The frames between `h` and `l` are held, and the one after `l`
    /// goes out carrying all of them — that is the mode's whole point.
    #[test]
    fn frames_are_held_for_the_length_of_an_update() {
        let t0 = Instant::now();
        let mut g = SyncGate::default();
        assert!(!g.hold_at(false, t0), "nothing to hold before the update");
        assert!(g.hold_at(true, t0));
        assert!(g.hold_at(true, t0 + Duration::from_millis(20)));
        assert!(
            !g.hold_at(false, t0 + Duration::from_millis(25)),
            "the closing `l` releases the frame"
        );
    }

    /// A program that never closes its update must not freeze the pane:
    /// a torn frame is a blemish, a frozen pane is a bug.
    #[test]
    fn an_update_left_open_stops_holding() {
        let t0 = Instant::now();
        let mut g = SyncGate::default();
        assert!(g.hold_at(true, t0));
        assert!(g.hold_at(true, t0 + SyncGate::MAX_HOLD - Duration::from_millis(1)));
        assert!(!g.hold_at(true, t0 + SyncGate::MAX_HOLD));
        assert!(
            !g.hold_at(true, t0 + SyncGate::MAX_HOLD + Duration::from_secs(9)),
            "once it has given up it must not start holding again"
        );
    }

    /// The next update after a release gets its own full budget.
    #[test]
    fn each_update_is_timed_from_its_own_start() {
        let t0 = Instant::now();
        let mut g = SyncGate::default();
        assert!(g.hold_at(true, t0));
        assert!(!g.hold_at(false, t0 + Duration::from_millis(10)));
        let t1 = t0 + Duration::from_secs(5);
        assert!(g.hold_at(true, t1), "a later update starts a fresh hold");
        assert!(g.hold_at(true, t1 + Duration::from_millis(20)));
    }
}

#[cfg(test)]
mod blank_gate_tests {
    use super::BlankGate;
    use std::time::{Duration, Instant};

    /// The wipe that precedes a repaint must not be shown.  codex
    /// clears the screen OUTSIDE its synchronized batch, so the blank
    /// grid between the wipe and the paint is what reached the display
    /// as a black flash.
    #[test]
    fn a_wipe_followed_by_a_repaint_is_never_shown() {
        let t0 = Instant::now();
        let mut g = BlankGate::default();
        assert!(g.hold_at(true, t0), "the blank screen waits");
        assert!(
            !g.hold_at(false, t0 + Duration::from_millis(3)),
            "the repaint publishes instead, and the blank is never seen"
        );
    }

    /// A screen that really is meant to be blank still appears.
    #[test]
    fn a_screen_meant_to_be_blank_still_appears() {
        let t0 = Instant::now();
        let mut g = BlankGate::default();
        assert!(g.hold_at(true, t0));
        assert!(!g.hold_at(true, t0 + BlankGate::MAX_HOLD));
        assert!(
            !g.hold_at(true, t0 + BlankGate::MAX_HOLD + Duration::from_secs(9)),
            "and it must not start waiting all over again"
        );
    }

    /// A withheld frame is owed until something is published, or a
    /// program that wipes the screen and goes quiet would leave the
    /// pane showing what it wiped.  The debt clears on discharge so a
    /// quiet pane goes back to sleep — idle CPU is a hard constraint.
    #[test]
    fn a_withheld_frame_is_owed_until_it_is_published() {
        let t0 = Instant::now();
        let mut g = BlankGate::default();
        assert!(!g.owes_a_frame(), "nothing held, nothing owed");
        g.hold_at(true, t0);
        assert!(g.owes_a_frame());
        g.discharged();
        assert!(!g.owes_a_frame(), "a quiet pane must not keep a timer alive");
    }
}

#[cfg(test)]
mod blank_cost {
    /// The blank test runs on every publish, so its cost is a hot-path
    /// cost and gets measured, not assumed.
    ///
    /// Measured on an M-series box: **16 ns** on a screen with content
    /// (it leaves at the first printable cell) against 30 µs for the
    /// full scan of a blank one — and a blank screen is exactly the
    /// case being held, at most a handful of calls across the 50 ms.
    #[test]
    fn the_blank_test_is_cheap_on_a_screen_with_content() {
        let mut t = marspot_term::terminal::Terminal::new(108, 33);
        for r in 0..33 {
            t.feed(format!("row {r} with some ordinary terminal content here\r\n").as_bytes());
        }
        assert!(!super::grid_is_blank(t.grid()), "this grid has content");

        let n = 20_000;
        let t0 = std::time::Instant::now();
        let mut sink = 0usize;
        for _ in 0..n {
            sink += super::grid_is_blank(t.grid()) as usize;
        }
        let per = t0.elapsed().as_nanos() as f64 / n as f64;
        assert_eq!(sink, 0);
        println!("grid_is_blank on a full 108x33 screen: {per:.0} ns/call");

        // A blank grid is the worst case: every cell is scanned.  Once
        // is enough — nothing asserts its speed, only the screen with
        // content does, and the 20 000 repetitions were half a second
        // spent producing a number that appears in a message
        // (2026-09-28).
        let blank = marspot_term::terminal::Terminal::new(108, 33);
        assert!(super::grid_is_blank(blank.grid()), "a blank grid reads as blank");

        // Measured 16 ns against 30 µs for the full scan — the bound
        // is loose enough to survive a loaded CI box and still catch a
        // regression to scanning every cell.
        assert!(
            per < 500.0,
            "a screen with content must leave on the first cell, not scan: {per} ns"
        );
    }
}

#[cfg(test)]
mod flood_gate_tests {
    use super::FloodGate;
    use std::time::{Duration, Instant};

    /// Turn after turn of full reads puts out one frame per display
    /// frame, not one per turn.
    #[test]
    fn a_flood_publishes_once_a_frame() {
        let t0 = Instant::now();
        let mut g = FloodGate::default();
        let mut frames = 0;
        // a turn every quarter millisecond for a tenth of a second
        for turn in 0..400u32 {
            if !g.hold_at(true, t0 + Duration::from_micros(250) * turn) {
                frames += 1;
            }
        }
        assert_eq!(frames, 13, "100 ms at one frame per 8 ms");
    }

    /// Output that is not a flood is never held, whatever went before.
    #[test]
    fn less_than_a_full_read_goes_out_at_once() {
        let t0 = Instant::now();
        let mut g = FloodGate::default();
        assert!(!g.hold_at(false, t0));
        assert!(!g.hold_at(false, t0 + Duration::from_micros(10)));
        assert!(!g.hold_at(true, t0 + Duration::from_secs(1)), "the first frame of a flood");
        assert!(g.hold_at(true, t0 + Duration::from_secs(1) + Duration::from_millis(1)));
        assert!(
            !g.hold_at(false, t0 + Duration::from_secs(1) + Duration::from_millis(2)),
            "the tail of the flood shows its last screen without waiting"
        );
        assert!(!g.owes_a_frame());
    }

    /// A flood that stops on a full read leaves a frame owed, and says
    /// when: the loop sleeps exactly that long and no longer.
    #[test]
    fn a_withheld_frame_is_due_one_frame_after_the_last() {
        let t0 = Instant::now();
        let mut g = FloodGate::default();
        assert_eq!(g.due_in_at(t0), None, "nothing held, no timer");
        assert!(!g.hold_at(true, t0));
        assert_eq!(g.due_in_at(t0), None);
        assert!(g.hold_at(true, t0 + Duration::from_millis(3)));
        assert!(g.owes_a_frame());
        assert_eq!(g.due_in_at(t0 + Duration::from_millis(3)), Some(Duration::from_millis(5)));
        assert_eq!(g.due_in_at(t0 + Duration::from_millis(30)), Some(Duration::ZERO));
        // the loop's timer fires with nothing parsed
        assert!(!g.hold_at(false, t0 + Duration::from_millis(8)));
        assert!(!g.owes_a_frame(), "a quiet pane must not keep a timer alive");
        assert_eq!(g.due_in_at(t0 + Duration::from_millis(9)), None);
    }

    /// A flood that stops on a full read has one more turn coming even
    /// when no frame is owed, and a quiet turn asks for no other.
    #[test]
    fn the_turn_after_a_flood_is_asked_for_once() {
        let t0 = Instant::now();
        let mut g = FloodGate::default();
        assert!(!g.hold_at(true, t0));
        g.turn(true);
        assert!(!g.owes_a_frame());
        assert_eq!(g.due_in_at(t0 + Duration::from_millis(2)), Some(Duration::from_millis(6)));
        g.turn(false);
        assert_eq!(g.due_in_at(t0 + Duration::from_millis(9)), None);
    }

    /// While an echo may be on its way no frame of the flood waits.
    #[test]
    fn nothing_is_held_right_after_a_key() {
        let mut g = FloodGate::default();
        let t0 = Instant::now();
        assert!(!g.hold_at(true, t0));
        assert!(g.hold_at(true, t0 + Duration::from_millis(1)));
        g.typed();
        let typed = Instant::now();
        assert!(!g.hold_at(true, typed + Duration::from_millis(1)));
        assert!(!g.hold_at(true, typed + Duration::from_millis(2)));
        assert!(!g.hold_at(true, typed + FloodGate::ECHO_WINDOW), "a frame is due by now anyway");
        assert!(g.hold_at(true, typed + FloodGate::ECHO_WINDOW + Duration::from_millis(1)));
    }
}
