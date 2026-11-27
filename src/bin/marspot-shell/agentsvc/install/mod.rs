//! The first install of an agent CLI waits for the person's yes.
//!
//! The installer asks here before it fetches anything for a CLI that is
//! missing.  The answer is in the settings, one per CLI: yes goes ahead,
//! no never does, and ask keeps a question here that the supervisor's
//! tick hands to the core, which shows it over the focused pane and
//! writes the answer back into the settings.  A question stays here
//! until the settings answer it, and every core that becomes the active
//! one is sent all of them again: one shown by a core that crashed or
//! was updated is not lost.  A CLI that is already there is never asked
//! about; after a no, marspot does not update it either.

use std::sync::Mutex;

use marspot::settings::{InstallConsent, InstallConsents, INSTALL_CLIS};
use marspot::shell_proto::InstallAsk;

/// What the installer does with a missing CLI this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    Install,
    /// Answered no, or a CLI the settings have no answer for.
    Leave,
    /// Not answered yet: the question is kept for the core.
    Asked,
}

/// The questions not answered yet, at most one per CLI, each with
/// whether the active core has been sent it.
static ASKS: Mutex<Vec<(InstallAsk, bool)>> = Mutex::new(Vec::new());

/// Whether the missing CLI `ask.cli` may be installed now; keeps the
/// question when nobody has answered yet.
pub(crate) fn gate(ask: InstallAsk, answers: &InstallConsents) -> Gate {
    match answers.of(&ask.cli) {
        Some(InstallConsent::Yes) => Gate::Install,
        Some(InstallConsent::No) | None => Gate::Leave,
        Some(InstallConsent::Ask) => {
            keep(ask);
            Gate::Asked
        }
    }
}

fn keep(ask: InstallAsk) {
    {
        let mut asks = ASKS.lock().unwrap_or_else(|p| p.into_inner());
        let full = asks.len() >= INSTALL_CLIS.len();
        match asks.iter_mut().find(|a| a.0.cli == ask.cli) {
            Some(a) if a.0 == ask => return,
            Some(a) => *a = (ask, false),
            None if !full => asks.push((ask, false)),
            None => return,
        }
    }
    crate::pump::knock();
}

/// The questions to send the core, oldest first: the ones it has not
/// been sent, or every one when `all` (a core that just became the
/// active one).  A question the settings no longer leave at ask is
/// dropped first.
pub(crate) fn to_send(answers: &InstallConsents, all: bool) -> Vec<InstallAsk> {
    let mut asks = ASKS.lock().unwrap_or_else(|p| p.into_inner());
    asks.retain(|a| answers.of(&a.0.cli) == Some(InstallConsent::Ask));
    asks.iter_mut()
        .filter(|a| all || !a.1)
        .map(|a| {
            a.1 = true;
            a.0.clone()
        })
        .collect()
}

/// Whether `now` answers yes for a CLI `before` did not: the installer
/// should look again at once rather than at its next pass.
pub(crate) fn newly_yes(before: &InstallConsents, now: &InstallConsents) -> bool {
    INSTALL_CLIS.iter().any(|c| now.of(c) == Some(InstallConsent::Yes) && before.of(c) != Some(InstallConsent::Yes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(cli: &str) -> InstallAsk {
        InstallAsk { cli: cli.into(), place: "~/.local/bin".into(), megabytes: 1 }
    }

    /// One test for the questions, which are process-wide.
    #[test]
    fn install_gate_keeps_one_question_per_cli_until_the_settings_answer_it() {
        to_send(&InstallConsents([InstallConsent::No; 4]), true);
        let mut answers = InstallConsents::default();
        answers.set("codex", InstallConsent::Yes);
        answers.set("kimi", InstallConsent::No);
        assert_eq!(gate(ask("codex"), &answers), Gate::Install);
        assert_eq!(gate(ask("kimi"), &answers), Gate::Leave);
        assert_eq!(gate(ask("npm"), &answers), Gate::Leave);
        assert!(to_send(&answers, false).is_empty(), "nothing asked for an answered CLI");
        assert_eq!(gate(ask("claude"), &answers), Gate::Asked);
        assert_eq!(gate(InstallAsk { megabytes: 2, ..ask("claude") }, &answers), Gate::Asked);
        assert_eq!(gate(ask("gemini"), &answers), Gate::Asked);
        let clis = |v: Vec<InstallAsk>| v.into_iter().map(|a| (a.cli, a.megabytes)).collect::<Vec<_>>();
        assert_eq!(clis(to_send(&answers, false)), [("claude".into(), 2), ("gemini".into(), 1)]);
        assert!(to_send(&answers, false).is_empty(), "sent once to this core");
        assert_eq!(gate(ask("gemini"), &answers), Gate::Asked);
        assert!(to_send(&answers, false).is_empty(), "the same question again is not sent again");
        assert_eq!(clis(to_send(&answers, true)).len(), 2, "a new core is sent every one still unanswered");
        answers.set("claude", InstallConsent::No);
        assert_eq!(clis(to_send(&answers, true)), [("gemini".into(), 1)], "an answer in the settings drops its question");
        answers.set("gemini", InstallConsent::Yes);
        assert!(to_send(&answers, true).is_empty());
    }

    #[test]
    fn install_gate_wakes_only_for_a_new_yes() {
        let before = InstallConsents::default();
        let mut now = before;
        assert!(!newly_yes(&before, &now));
        now.set("gemini", InstallConsent::No);
        assert!(!newly_yes(&before, &now), "a no installs nothing");
        now.set("claude", InstallConsent::Yes);
        assert!(newly_yes(&before, &now));
        assert!(!newly_yes(&now, &now), "already yes");
    }
}
