//! The question the shell asks before it installs an agent CLI for the
//! first time: which CLI, where it goes and how much room it takes,
//! over the focused pane of every window until it is answered.
//!
//! It comes up on its own, while the person may be typing into a pane,
//! so no key that types anything answers it: Ctrl-Option-Y installs and
//! Ctrl-Option-N never installs, each written at once into the settings
//! for that CLI; the shell's installer reads the settings and acts on
//! them.  Every other key, Esc included, and every input-method
//! composition and commit go to the pane as they would without it.  An
//! answer given in the settings panel or in the settings file takes it
//! down too.

use super::*;
use marspot::input::{KeyState, LogicalKey};
use marspot::settings::{InstallConsent, InstallConsents, INSTALL_CLIS};
use marspot::shell_proto::InstallAsk;

/// One question, as the person reads it.
pub(super) struct Asked {
    cli: String,
    text: String,
}

/// The questions waiting, the first shown; at most one per CLI.
#[derive(Default)]
pub(super) struct InstallAsks {
    up: Vec<Asked>,
    /// When the shown question went up, for the log.
    pub(super) up_since: Option<Instant>,
}

impl InstallAsks {
    /// Take a question unless it was answered already.  The same CLI
    /// asked again replaces its text.
    pub(super) fn offer(&mut self, cli: &str, text: String, answers: &InstallConsents) -> bool {
        if answers.of(cli) != Some(InstallConsent::Ask) {
            return false;
        }
        let full = self.up.len() >= INSTALL_CLIS.len();
        match self.up.iter_mut().find(|a| a.cli == cli) {
            Some(a) => a.text = text,
            None if !full => self.up.push(Asked { cli: cli.to_string(), text }),
            None => return false,
        }
        self.up_since.get_or_insert_with(Instant::now);
        true
    }

    pub(super) fn shown(&self) -> Option<&str> {
        self.up.first().map(|a| a.text.as_str())
    }

    fn shown_cli(&self) -> Option<&str> {
        self.up.first().map(|a| a.cli.as_str())
    }

    /// Drop the questions the settings now answer; whether any went.
    pub(super) fn prune(&mut self, answers: &InstallConsents) -> bool {
        let first = self.shown_cli().map(str::to_string);
        let before = self.up.len();
        self.up.retain(|a| answers.of(&a.cli) == Some(InstallConsent::Ask));
        if self.shown_cli() != first.as_deref() {
            self.up_since = (!self.up.is_empty()).then(Instant::now);
        }
        self.up.len() != before
    }
}

/// The answer a pressed key gives: only Ctrl-Option-Y or Ctrl-Option-N,
/// with neither Shift nor Command.  `logical` is the key's own
/// character whatever the modifiers type.
pub(super) fn answer_for(event: &MarspotKeyEvent, mods: Modifiers) -> Option<InstallConsent> {
    if !(mods.control && mods.alt) || mods.shift || mods.super_ {
        return None;
    }
    match event.logical {
        LogicalKey::Char('y') => Some(InstallConsent::Yes),
        LogicalKey::Char('n') => Some(InstallConsent::No),
        _ => None,
    }
}

impl CoreApp {
    /// The shell asks before installing `ask.cli`.
    pub(super) fn install_ask(&mut self, ask: InstallAsk) {
        let text = strings::text(Arg::InstallAsk { cli: &ask.cli, place: &ask.place, megabytes: ask.megabytes });
        if self.notices.install.offer(&ask.cli, text, &marspot::settings::get().install_consent) {
            lx_event!("INSTALL_ASK_SHOWN", "the question before an agent CLI's first install is up", cli = ask.cli.as_str());
            self.windows.iter_mut().for_each(|w| w.needs_render = true);
        }
    }

    /// A key for window `wi`: whether the question took it.  Its
    /// release reaches nobody; every other key is left alone.
    pub(super) fn install_ask_key(&mut self, wi: usize, event: &MarspotKeyEvent, mods: Modifiers) -> bool {
        if event.state != KeyState::Pressed || self.notices.install.shown().is_none() {
            return false;
        }
        if self.strip_has_keys(self.windows[wi].window_id) {
            return false;
        }
        let Some(to) = answer_for(event, mods) else { return false };
        self.pane_control.shadows.peeks.held.swallowed(&event.logical);
        if !event.repeat {
            self.install_answer(to);
        }
        true
    }

    fn install_answer(&mut self, to: InstallConsent) {
        let q = &mut self.notices.install;
        let Some(cli) = q.shown_cli().map(str::to_string) else { return };
        let up_ms = q.up_since.map_or(0, |t| t.elapsed().as_millis() as u64);
        let mut next = (*marspot::settings::get()).clone();
        next.install_consent.set(&cli, to);
        match marspot::settings::write(&next) {
            Ok(()) => drop(marspot::settings::reload_if_changed()),
            Err(e) => lx_warn!("core.install_ask.write_failed", &e.to_string(), cli = cli.as_str()),
        }
        lx_event!("INSTALL_ASK_ANSWERED", "a chord answered the question before an agent CLI's first install", cli = cli.as_str(), answer = format!("{to:?}"), up_ms = up_ms);
        self.install_ask_tick();
    }

    /// Once per loop turn while a question is up: one answered by a
    /// chord, in the settings panel or in the settings file comes down.
    pub(super) fn install_ask_tick(&mut self) {
        if self.notices.install.shown().is_some() && self.notices.install.prune(&marspot::settings::get().install_consent) {
            self.windows.iter_mut().for_each(|w| w.needs_render = true);
        }
    }
}

#[cfg(test)]
#[path = "install_ask_tests.rs"]
mod tests;
