//! A claude helper at an approval prompt, known within seconds: its
//! transcript ends with a tool call no result follows and no process
//! runs, and its screen shows numbered choices with no input box.  The
//! pane's composed state says the same only once nothing was drawn for
//! half a minute, and a parent told that late, or told twice, waits.
//!
//! The screen is read on the helpers' worker (`Job::ReadPrompt`), never
//! on the main loop's tick: a read opens the session's entry and frame.

use super::notify;
use super::{Shared, State};
use crate::plugins::claudecode::with_helpers::turns::Look;

/// How long after its record a tool call's prompt is looked for: claude
/// draws it a moment after the record.
const DRAWN_MS: u64 = 1_000;
/// How long after a screen with no prompt on it it is read again: a tool
/// that runs in claude itself (an MCP server's, a subagent) may ask later.
const REREAD_MS: u64 = 2_000;
/// How long after a screen that showed the prompt it is read again: an
/// answer to a tool that then runs in claude itself leaves the call open
/// with no record, and may draw nothing the shell hears of.
const SHOWN_REREAD_MS: u64 = 5_000;

/// What the screen showed while the transcript was at one tool call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Asked {
    /// When the call was recorded (ms after the epoch).
    since: u64,
    /// The call asks the user a question of claude's own.
    question: bool,
    shown: bool,
    /// When the last read that showed the prompt was (ms after the epoch).
    shown_at: u64,
    /// When the screen is read again (ms after the epoch).
    next_ms: u64,
    /// A read is queued to the worker.
    reading: bool,
}

impl Asked {
    /// At a question claude asked, rather than at an approval.
    pub(super) fn question(&self) -> bool {
        self.question
    }
}

impl Shared {
    /// Whether helper `sid`, whose transcript says `look`, is at a prompt
    /// by its screen as last read, now `now_ms`; `drawn_ms`: when its pane
    /// last printed, if the shell heard.  `read` queues a read of the
    /// screen at the call recorded at its argument to the worker (false
    /// when it did not go): at most one at a
    /// time, once in [`REREAD_MS`] while no prompt was seen and once in
    /// [`SHOWN_REREAD_MS`] while one was.  A prompt seen counts no more
    /// once the pane printed since: it is read again first.
    ///
    /// A call that ends (another record follows it, or another call) is
    /// an answer: the prompt told of for it says nothing of the next.  A
    /// process that runs under claude at the same call is none: a hook or
    /// a status line command starts while claude waits at the prompt.
    pub(super) fn at_prompt(&self, sid: u64, look: Option<Look>, now_ms: u64, drawn_ms: Option<u64>, read: impl FnOnce(u64) -> bool) -> bool {
        let mut st = self.lock();
        let (since, question) = match look {
            // no recent scan: what was known stands
            None => return false,
            Some(Look::Asking { since, question }) => (since, question),
            Some(Look::Running { since }) => {
                if let Some(a) = at_call(&mut st, sid, Some(since)) {
                    // whatever ran may have answered it: read it first
                    a.next_ms = now_ms;
                }
                return false;
            }
            Some(Look::Over { .. } | Look::NotOver) => {
                at_call(&mut st, sid, None);
                return false;
            }
        };
        at_call(&mut st, sid, Some(since));
        if now_ms.saturating_sub(since) < DRAWN_MS {
            return false;
        }
        let a = st.asking.entry(sid).or_insert(Asked { since, question, ..Asked::default() });
        if a.shown && drawn_ms.is_some_and(|d| d > a.shown_at) {
            a.shown = false;
            a.next_ms = now_ms;
        }
        let (shown, due) = (a.shown, !a.reading && now_ms >= a.next_ms);
        // marked before it goes: the worker may finish the read before
        // `read` returns
        a.reading |= due;
        drop(st);
        if due && !read(since)
            && let Some(a) = self.lock().asking.get_mut(&sid).filter(|a| a.since == since)
        {
            a.reading = false;
        }
        shown
    }

    /// Read helper `sid`'s screen at the call recorded at `since`: for the
    /// worker only.  Timed from before the read: output drawn while it
    /// reads is newer than what it saw.
    pub(super) fn read_prompt(&self, sid: u64, since: u64) {
        let at_ms = super::records::now_ms();
        self.prompt_read(sid, since, prompt_on_screen(sid), at_ms);
    }

    /// What a read of helper `sid`'s screen, queued at the call recorded
    /// at `since` and begun at `now_ms`, found; a read for a call since
    /// answered says nothing.
    pub(super) fn prompt_read(&self, sid: u64, since: u64, shown: bool, now_ms: u64) {
        let mut st = self.lock();
        let Some(a) = st.asking.get_mut(&sid).filter(|a| a.since == since) else { return };
        let changed = a.shown != shown;
        a.reading = false;
        a.shown = shown;
        a.next_ms = now_ms + if shown { SHOWN_REREAD_MS } else { REREAD_MS };
        if shown {
            a.shown_at = now_ms;
        }
        drop(st);
        if changed && let Some(h) = crate::pane_control::hub() {
            h.knock();
        }
    }

    /// Reads queued to a worker that is gone: none is under way.
    pub(super) fn reads_lost(&self) {
        for a in self.lock().asking.values_mut() {
            a.reading = false;
        }
    }
}

/// Helper `sid`'s transcript is at the call recorded at `since`, or at
/// none: a call known before that is not this one is answered.  The call
/// still known, if any.
fn at_call(st: &mut State, sid: u64, since: Option<u64>) -> Option<&mut Asked> {
    if st.asking.get(&sid).is_some_and(|a| Some(a.since) != since) {
        st.asking.remove(&sid);
        st.answered.insert(sid);
    }
    st.asking.get_mut(&sid)
}

/// Whether pane `sid` shows a prompt waiting for an answer.  Reads the
/// session's entry and frame: for the worker only.
pub(super) fn prompt_on_screen(sid: u64) -> bool {
    let boxed = !matches!(marspot::pane_read::composer_box(sid), Ok(None));
    super::tell::screen_of(sid).is_some_and(|s| asks(boxed, &s))
}

/// Whether `screen`, with an input box on it or not (`boxed`), waits for
/// an answer: no box, and numbered choices at the bottom.
pub(super) fn asks(boxed: bool, screen: &str) -> bool {
    !boxed && notify::choices_at_bottom(screen)
}

#[cfg(test)]
#[path = "tests_asking.rs"]
mod tests;
