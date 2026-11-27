//! Sentences shown over a pane: the one the shell sends (`Notice`),
//! over the focused pane of every window until it has been up long
//! enough to read, and the question a held `/model` asks over its own
//! pane until it is answered (`model_guard`).
//!
//! Nothing ticks for it: the loop's wait includes the moment the
//! notice comes down, and with none up there is nothing to wait for.

use super::*;

/// How long the shell's notice stays up.
pub(super) const SHOWN_FOR: Duration = Duration::from_secs(12);

#[derive(Default)]
pub(super) struct Notices {
    shell: Option<(String, Instant)>,
    pub(super) guard: Option<super::model_guard::Held>,
    /// The window whose input-method composition of y answered the
    /// guard, and when that composition ended (none while it is still
    /// up): until then, and for the key right after, what it shows and
    /// commits is dropped.
    pub(super) composed_yes: Option<(u32, Option<Instant>)>,
    /// What programs' input left unsent in a Claude Code box, by
    /// session, until the moment it no longer stands for what the box
    /// holds (`model_guard::UNSENT_STANDS`); at most `UNSENT_CAP`.
    pub(super) unsent: Vec<(u64, marspot::pane_read::slash_model::Typed, Instant)>,
}

impl Notices {
    /// The shell's notice, until `now` reaches its end.
    pub(super) fn shell_text(&self) -> Option<&str> {
        self.shell.as_ref().map(|(t, _)| t.as_str())
    }

    /// What is shown over the pane showing session `sid`, if anything.
    pub(super) fn over(&self, sid: Option<u64>, focused: bool) -> Option<&str> {
        match &self.guard {
            Some(held) if sid == Some(held.sid) => Some(marspot::ui::strings::t(marspot::ui::strings::Msg::ModelGuardNotice)),
            _ => self.shell_text().filter(|_| focused),
        }
    }

    /// How long until a notice comes down.
    pub(super) fn wait(&self, now: Instant) -> Option<Duration> {
        self.shell.as_ref().map(|(_, until)| until.saturating_duration_since(now))
    }

    /// Takes down a notice whose time is up; true when one was.
    fn expire(&mut self, now: Instant) -> bool {
        if self.shell.as_ref().is_some_and(|(_, until)| *until <= now) {
            self.shell = None;
            return true;
        }
        false
    }
}

impl CoreApp {
    /// The shell asked for `text` to be read.
    pub(super) fn show_notice(&mut self, text: String) {
        lx_event!("NOTICE_SHOWN", "a notice from the shell is shown over the focused pane", text = text.as_str());
        self.notices.shell = Some((text, Instant::now() + SHOWN_FOR));
        self.windows.iter_mut().for_each(|w| w.needs_render = true);
    }

    /// Once per loop turn: a notice whose time is up comes down.
    pub(super) fn notices_tick(&mut self) {
        if self.notices.expire(Instant::now()) {
            lx_event!("NOTICE_GONE", "the shell's notice came down");
            self.windows.iter_mut().for_each(|w| w.needs_render = true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shells_notice_comes_down_when_its_time_is_up_and_nothing_waits_after() {
        let mut n = Notices::default();
        let t0 = Instant::now();
        assert_eq!(n.wait(t0), None);
        n.shell = Some(("hello".into(), t0 + SHOWN_FOR));
        assert_eq!(n.wait(t0), Some(SHOWN_FOR));
        assert!(!n.expire(t0 + SHOWN_FOR / 2));
        assert_eq!(n.shell_text(), Some("hello"));
        assert!(n.expire(t0 + SHOWN_FOR));
        assert_eq!((n.shell_text(), n.wait(t0)), (None, None));
    }
}
