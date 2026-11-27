//! The installer against real scripts in a scratch home: nothing is
//! fetched for a CLI until its answer is yes.

use std::path::{Path, PathBuf};

use marspot::settings::{InstallConsent, InstallConsents};

use super::super::{missing, pass_at, Cli, Failures, Places};
use super::ask_for;
use crate::agentsvc::install::to_send;

/// The CLIs a new core would be asked about.
fn asked(answers: &InstallConsents) -> Vec<String> {
    to_send(answers, true).into_iter().map(|a| a.cli).collect()
}

/// A scratch home, and installer scripts that each leave a mark that
/// they ran and put a stand-in command where the vendor's would go.
fn scene() -> (Places, PathBuf) {
    let root = std::env::temp_dir().join(format!("marspot-consent-{}", marspot_peer::rand::hex(6)));
    let home = root.join("home");
    let fixtures = root.join("fixtures");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&fixtures).unwrap();
    for (cli, dir) in [("claude", ".local/bin"), ("codex", ".local/bin"), ("gemini", ".local/bin"), ("kimi", ".kimi-code/bin")] {
        let script = format!(
            "mkdir -p \"$HOME/{dir}\" && touch \"$HOME/ran-{cli}\" && printf '#!/bin/sh\\necho 0.0.0\\n' > \"$HOME/{dir}/{cli}\" && chmod +x \"$HOME/{dir}/{cli}\"\n"
        );
        std::fs::write(fixtures.join(format!("install-{cli}.sh")), script).unwrap();
    }
    let at = Places { path: root.join("bin").into_os_string(), codex_home: home.join(".codex"), state: root.join("state"), home };
    (at, fixtures)
}

fn ran(home: &Path, cli: &str) -> bool {
    home.join(format!("ran-{cli}")).exists()
}

#[test]
fn install_consent_nothing_runs_before_a_yes_and_a_yes_installs_that_cli_only() {
    let (at, fixtures) = scene();
    to_send(&InstallConsents([InstallConsent::No; 4]), true);
    let mut answers = InstallConsents::default();
    answers.set("kimi", InstallConsent::No);
    pass_at(&at, &answers, Some(&fixtures), &mut Failures::default());
    for cli in ["claude", "codex", "gemini", "kimi"] {
        assert!(!ran(&at.home, cli), "{cli}'s installer ran with no yes");
    }
    assert_eq!(missing(&at), Cli::ALL);
    assert_eq!(asked(&answers), ["claude", "codex", "gemini"], "asked for each unanswered CLI, not for the one answered no");

    answers.set("codex", InstallConsent::Yes);
    pass_at(&at, &answers, Some(&fixtures), &mut Failures::default());
    assert!(ran(&at.home, "codex"), "a yes installs");
    assert_eq!(missing(&at), [Cli::Claude, Cli::Gemini, Cli::Kimi]);
    assert!(!ran(&at.home, "claude") && !ran(&at.home, "gemini") && !ran(&at.home, "kimi"));
    assert_eq!(asked(&answers), ["claude", "gemini"], "an answered CLI is not asked again");

    // the CLI is there now: a later pass, whatever the answer, leaves it
    std::fs::remove_file(at.home.join("ran-codex")).unwrap();
    answers.set("codex", InstallConsent::No);
    pass_at(&at, &answers, Some(&fixtures), &mut Failures::default());
    assert!(!ran(&at.home, "codex"));
    std::fs::remove_dir_all(at.home.parent().unwrap()).unwrap();
}

#[test]
fn install_consent_names_where_each_installer_puts_its_files() {
    let (at, _) = scene();
    let place = |cli| ask_for(cli, &at).place;
    assert_eq!(place(Cli::Claude), "~/.local/share/claude + ~/.local/bin");
    assert_eq!(place(Cli::Codex), "~/.codex/packages + ~/.local/bin");
    assert_eq!(place(Cli::Kimi), "~/.kimi-code");
    let elsewhere = Places { codex_home: "/opt/codex".into(), ..at };
    assert_eq!(ask_for(Cli::Codex, &elsewhere).place, "/opt/codex/packages + ~/.local/bin", "CODEX_HOME outside home");
    std::fs::remove_dir_all(elsewhere.home.parent().unwrap()).unwrap();
}
