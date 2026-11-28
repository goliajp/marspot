//! Which reset lines are buttons, and the last claim on its line.

use super::*;
use marspot::agent_resets::{Claimed, Grant};

const NOW: i64 = 1_795_740_000;

fn account(agent: &str, kinds: &[&str], observed_at: i64) -> AccountResets {
    let grants = kinds.iter().map(|k| Grant { kind: k.to_string(), left: Some(1), total: Some(1), ends_at: Some(NOW + 86_400), ..Grant::default() }).collect();
    AccountResets { agent: agent.into(), profile: 2, name: format!("{agent} 2"), resets: Resets { grants, ..Resets::none(observed_at, "devops_feed") } }
}

fn modal(claims: Vec<Claimed>) -> CcUsageModalState {
    CcUsageModalState { data: None, codex: None, resets: Vec::new(), claims, buttons: Default::default(), loaded_at: Instant::now(), surface: Default::default() }
}

#[test]
fn reset_panel_a_stored_claude_grant_is_no_button_and_a_codex_credit_is() {
    let m = modal(Vec::new());
    let claude = lines_for(&m, Some(&account("claude", &["claude_full", "claude_fable"], NOW - 60)), &[], NOW);
    assert_eq!(claude.len(), 2);
    assert!(claude.iter().all(|l| !l.pressable), "no claim path for them: {claude:?}");
    let session = lines_for(&m, Some(&account("claude", &["session"], NOW - 60)), &[], NOW);
    assert!(session[0].pressable, "the session reset is claimed through the CLI");
    let codex = lines_for(&m, Some(&account("codex", &["codex_full"], NOW - 60)), &[], NOW);
    assert!(codex[0].pressable);
}

#[test]
fn reset_panel_the_last_claim_shows_on_its_line_until_a_newer_reading() {
    let claim = |at: i64| Claimed { at, agent: "codex".into(), profile: 2, kind: "codex_full".into(), result: "not_limited".into(), detail: "nothingToReset".into() };
    let a = account("codex", &["other", "codex_full"], NOW - 60);
    let lines = lines_for(&modal(vec![claim(NOW - 30)]), Some(&a), &[], NOW);
    assert_eq!(lines[1].text, "full not_limited: nothingToReset");
    assert!(lines[0].text.starts_with("other "), "only the line of that kind: {lines:?}");
    let read_since = lines_for(&modal(vec![claim(NOW - 90)]), Some(&a), &[], NOW);
    assert!(read_since[1].text.starts_with("full 1/1"), "a newer reading clears it: {read_since:?}");
    let other = Claimed { profile: 3, ..claim(NOW - 30) };
    assert!(lines_for(&modal(vec![other]), Some(&a), &[], NOW)[1].text.starts_with("full 1/1"), "another account's claim");
}
