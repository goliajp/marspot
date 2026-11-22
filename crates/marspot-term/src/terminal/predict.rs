//! Local-echo prediction: painting a typed byte before the PTY echoes
//! it, timing the echo, and taking a guess back when it was wrong or
//! never answered.

use super::{Prediction, Terminal};
use crate::grid::Cell;
use std::time::{Duration, Instant};

impl Terminal {
    /// Three wasted keystrokes is a small, bounded amount of flicker
    /// to pay to find out that a program does not echo.
    pub(super) const PREDICT_GIVE_UP_AFTER: u32 = 3;
    /// And one every so often to find out whether it has changed —
    /// the pane a program was quit in becomes a shell again, and
    /// nothing else announces that.  Rare enough that the stray
    /// character is not the thing anyone notices.
    pub(super) const PREDICT_PROBE_EVERY: u32 = 128;

    /// Starting guess for the echo round trip.  Measured 2026-09-07
    /// against a real zsh on a pty: idle p50 0.16 ms / max 1.12 ms,
    /// and 0.16 ms / 8.18 ms with the shell flooding the pipe at the
    /// same time.  10 ms is comfortably above all of it and is only
    /// a seed — `echo_srtt` moves to whatever the link actually is.
    pub(super) const ECHO_SRTT_SEED: Duration = Duration::from_millis(10);
    /// A prediction is abandoned after this long unanswered.  Four
    /// round trips, floored well above local pty latency and capped
    /// so that even a very slow link cannot leave a character from
    /// an unechoed password sitting on screen for a whole second.
    pub(super) const PREDICT_DEADLINE_MIN: Duration = Duration::from_millis(50);
    pub(super) const PREDICT_DEADLINE_MAX: Duration = Duration::from_millis(1000);

    /// How long an unanswered prediction is given before it is taken
    /// back off the screen.
    pub fn predict_deadline(&self) -> Duration {
        (self.echo_srtt * 4).clamp(Self::PREDICT_DEADLINE_MIN, Self::PREDICT_DEADLINE_MAX)
    }

    /// True while at least one guess is still on screen unconfirmed —
    /// the caller's cue to come back and check the deadline rather
    /// than sleep until the next byte, which may never come.
    pub fn predictions_pending(&self) -> bool {
        !self.predictions.is_empty()
    }

    /// Take back every prediction that has gone unanswered past the
    /// deadline.  Their bytes move to the shadow queue: whether the
    /// program was slow or is simply never going to echo is decided
    /// by what arrives next, not here.  Returns true iff the grid
    /// changed (caller should redraw).
    pub fn expire_predictions_at(&mut self, now: Instant) -> bool {
        let deadline = self.predict_deadline();
        match self.predictions.front() {
            Some(p) if now.duration_since(p.at) >= deadline => {}
            _ => return false,
        }
        const SHADOW_MAX: usize = 64;
        for p in &self.predictions {
            if self.shadow.len() >= SHADOW_MAX {
                self.shadow.pop_front();
            }
            self.shadow.push_back((p.byte, p.at));
        }
        self.predictions_expired += self.predictions.len() as u64;
        self.rollback_predictions_inner(false);
        true
    }

    /// Wall-clock wrapper over [`Self::expire_predictions_at`].
    pub fn expire_predictions(&mut self) -> bool {
        self.expire_predictions_at(Instant::now())
    }

    /// Fold one observed keystroke-to-echo round trip into the
    /// smoothed estimate (the usual 7/8 EWMA).
    pub(super) fn note_echo_rtt(&mut self, sample: Duration) {
        self.echo_srtt = (self.echo_srtt * 7 + sample) / 8;
    }

    /// True iff local-echo prediction is currently worth making.
    ///
    /// Alt screen is refused outright: vim / less / htop do not echo
    /// keys verbatim.  Beyond that the terminal does not guess what
    /// KIND of program is on the other end — it reads what happened
    /// to the last few guesses.  termios cannot answer this: a zsh
    /// prompt is `-icanon -echo` exactly like codex, because ZLE
    /// draws its own line too.  The difference is only WHERE, and the
    /// tally is the only thing that sees it.
    pub fn can_predict(&self) -> bool {
        self.saved_main.is_none()
            && (self.predict_misses < Self::PREDICT_GIVE_UP_AFTER
                || self.predict_declined >= Self::PREDICT_PROBE_EVERY)
    }

    /// Try to local-echo `byte`: if it's printable ASCII and we're
    /// in cooked-echo territory, write it to the grid + advance the
    /// cursor + queue a prediction so the matching PTY echo will be
    /// silently consumed.  Returns true when the prediction was
    /// applied (caller should request a redraw).
    ///
    /// We deliberately predict only `0x20..=0x7E`.  Other bytes have
    /// shell-side side effects we can't model from the keystroke
    /// alone: `\r` becomes `\r\n` under ONLCR, Tab triggers
    /// completion, Ctrl-* delivers signals, etc.
    pub fn predict_byte(&mut self, byte: u8) -> bool {
        if !self.can_predict() {
            self.predict_declined = self.predict_declined.saturating_add(1);
            return false;
        }
        // Taking the probe: whatever it teaches, it is spent.
        self.predict_declined = 0;
        if !(0x20..=0x7E).contains(&byte) {
            return false;
        }
        let cols = self.grid.cols();
        let (col, row) = self.grid.cursor();
        // Decline in the last column and on a pending wrap: an echo
        // consumed as a hit never reaches the parser to set or take it
        if self.pending_wrap || col + 1 >= cols {
            return false;
        }
        let saved_cell = self.grid.cell(col, row);
        let saved_cursor = (col, row);
        self.grid.set_cell(
            col,
            row,
            Cell {
                ch: byte as char,
                attrs: self.attrs,
            },
        );
        if col + 1 < cols {
            self.grid.set_cursor(col + 1, row);
        }
        // Cap the queue so a runaway typing session can't grow it
        // unbounded if echoes never come.  64 is generous — typical
        // round-trip is one byte before the next key.
        const MAX_PREDICTIONS: usize = 64;
        if self.predictions.len() >= MAX_PREDICTIONS {
            self.rollback_predictions();
            return false;
        }
        self.predictions.push_back(Prediction {
            byte,
            saved_cell,
            saved_cursor,
            at: Instant::now(),
        });
        true
    }

    /// Reverse-pop every pending prediction, restoring its saved
    /// cell + cursor.  After this, the grid is back to whatever it
    /// looked like before the first un-confirmed prediction.
    pub(super) fn rollback_predictions(&mut self) {
        self.rollback_predictions_inner(true);
    }

    /// The unpainting itself.  `count_miss` separates "the program
    /// echoed something else" (a miss) from "nothing came back yet"
    /// (an expiry) — same pixels restored, different verdict.
    fn rollback_predictions_inner(&mut self, count_miss: bool) {
        while let Some(p) = self.predictions.pop_back() {
            self.grid
                .set_cell(p.saved_cursor.0, p.saved_cursor.1, p.saved_cell);
            self.grid.set_cursor(p.saved_cursor.0, p.saved_cursor.1);
            if count_miss {
                self.predictions_miss += 1;
            }
        }
    }
}
