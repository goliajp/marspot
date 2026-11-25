//! What the codex adapter must not take for a pane's own: a reading of
//! an earlier codex, a refusal heard once and never cleared, a heads-up
//! already given, a rollout another pane in the same directory writes;
//! and what it must not merge: two refusals of two turns.

use std::fs;
use std::path::PathBuf;

use super::alloc::{CodexAlloc, Snapshot};
use super::alloc_rollout::{made_at, rollout_of_thread};
use super::alloc_tests::{H, NOW, START, accounts, append, refused_line, rows, snap, tokens_line, turn, world};
use super::alloc_wire::{Seen, cwd_rollout};
use marspot::pidtree::ProcId;
use marspot_term::test_env::TempDir;

fn plans(log: &[(&'static str, String)]) -> Vec<String> {
    log.iter().filter(|(t, _)| *t == "codex.alloc.plan").map(|(_, l)| l.clone()).collect()
}

/// After an account switch the rollout's tail still holds the account
/// it ran on before; a reading from before this codex started is not
/// this account's.
#[test]
fn codex_alloc_reading_before_the_tui_started_is_not_taken() {
    let w = world("codex-alloc-limits-before");
    append(&w.rollout, &[tokens_line(START - 5, 99.0, START + 40 * H)]);
    let mut s = snap(&w, START + 600, false);
    for ac in &mut s.accounts {
        ac.measured_at = START - 60;
    }
    let (_, tell) = CodexAlloc::new(w.dir.clone()).observe(&s);
    assert!(tell.is_empty(), "{tell:?}");
}

/// A pane the peerbus heard refused, with no rollout to read, is not
/// refused any more once the peerbus hears a turn of it end well.
#[test]
fn codex_alloc_peerbus_good_turn_ends_a_refusal() {
    let w = world("codex-alloc-peer-done");
    let mut a = CodexAlloc::new(w.dir.clone());
    let mut s = snap(&w, NOW, false);
    s.panes[0].rollout = None;
    s.panes[0].peer.refused = Some((NOW - 30, Some(turn(1))));
    let (log, _) = a.observe(&s);
    assert_eq!(plans(&log), ["7→P2(out)"]);
    let mut later = snap(&w, NOW + 60, false);
    later.panes[0].rollout = None;
    later.panes[0].peer = s.panes[0].peer.clone();
    later.panes[0].peer.done_at = Some(NOW + 50);
    let (log, _) = a.observe(&later);
    assert_eq!(plans(&log), ["no pane would move"]);
}

/// A heads-up is given once per account and week, though the pane is
/// gone for a scan, and though a restart reads its going back.
#[test]
fn codex_alloc_heads_up_survives_a_gone_pane_and_a_restart() {
    let w = world("codex-alloc-told-gone");
    append(&w.rollout, &[tokens_line(NOW - 60, 99.8, NOW + 40 * H)]);
    let mut a = CodexAlloc::new(w.dir.clone());
    assert_eq!(a.observe(&snap(&w, NOW, false)).1.len(), 1);
    a.observe(&Snapshot { panes: Vec::new(), ..snap(&w, NOW + 60, false) });
    assert!(a.observe(&snap(&w, NOW + 120, false)).1.is_empty(), "back after a scan away");
    a.observe(&Snapshot { panes: Vec::new(), ..snap(&w, NOW + 180, false) });
    assert!(rows(&w.dir, "bound").iter().any(|r| r[4] == "-"), "the pane's going is written");
    assert!(CodexAlloc::new(w.dir.clone()).observe(&snap(&w, NOW + 240, false)).1.is_empty(), "nor after a restart");
}

/// Two turns refused within a minute are two refusals; one turn heard
/// again later is still one.
#[test]
fn codex_alloc_refusals_are_told_apart_by_turn() {
    let w = world("codex-alloc-by-turn");
    let mut a = CodexAlloc::new(w.dir.clone());
    a.observe(&snap(&w, NOW, false));
    append(&w.rollout, &[refused_line(NOW + 5, 1), refused_line(NOW + 20, 2)]);
    a.observe(&snap(&w, NOW + 30, false));
    assert_eq!(rows(&w.dir, "refused").len(), 2);
    let mut s = snap(&w, NOW + 140, false);
    s.panes[0].peer.refused = Some((NOW + 130, Some(turn(2))));
    a.observe(&s);
    assert_eq!(rows(&w.dir, "refused").len(), 2, "turn 2 heard from the peerbus too");
}

/// Unchanged inputs are not decided again until a minute has passed.
#[test]
fn codex_alloc_unchanged_inputs_are_not_decided_again() {
    let w = world("codex-alloc-same-inputs");
    let mut a = CodexAlloc::new(w.dir.clone());
    assert_eq!(plans(&a.observe(&snap(&w, NOW, false)).0).len(), 1);
    a.said = None;
    assert!(plans(&a.observe(&snap(&w, NOW, false)).0).is_empty(), "decided again");
    a.said = None;
    assert_eq!(plans(&a.observe(&snap(&w, NOW + 60, false)).0).len(), 1);
}

#[test]
fn codex_alloc_idle_pane_and_single_account_are_not_told() {
    let w = world("codex-alloc-not-told");
    append(&w.rollout, &[tokens_line(NOW - 60, 99.8, NOW + 40 * H)]);
    assert!(CodexAlloc::new(w.dir.clone()).observe(&snap(&w, NOW, true)).1.is_empty(), "an idle pane");
    let w = world("codex-alloc-one-account");
    append(&w.rollout, &[tokens_line(NOW - 60, 99.8, NOW + 40 * H)]);
    let mut s = snap(&w, NOW, false);
    s.accounts = accounts(NOW).into_iter().filter(|a| a.profile == 1).collect();
    assert!(CodexAlloc::new(w.dir.clone()).observe(&s).1.is_empty(), "nowhere else to go");
}

#[test]
fn codex_alloc_shared_directory_gets_no_rollout_by_directory() {
    let seen = |sid: u64, cwd: &str| Seen {
        sid,
        codex: ProcId { pid: sid as i32, start_us: 0 },
        home: PathBuf::new(),
        cwd: Some(cwd.into()),
        cwd_rollout: Some(PathBuf::from(format!("/r{cwd}.jsonl"))),
        model: None,
        profile: Some(1),
    };
    let all = [seen(1, "/w"), seen(2, "/w"), seen(3, "/x")];
    assert_eq!(cwd_rollout(&all, &all[0]), None);
    assert_eq!(cwd_rollout(&all, &all[1]), None);
    assert_eq!(cwd_rollout(&all, &all[2]), Some(PathBuf::from("/r/x.jsonl")));
}

/// A thread's rollout is looked for by the day its id was made, in the
/// local day's directory, and nowhere far from it.
#[test]
fn codex_alloc_thread_rollout_is_found_by_its_day() {
    let thread = "01a0a5da-da3c-7c02-9ad9-000000000001";
    assert_eq!(made_at(thread), Some(1_789_488_978)); // 2026-09-15 16:16:18 UTC
    assert_eq!(made_at("01a0a5da-da3c-4c02-9ad9-000000000001"), None, "not a UUIDv7");
    let home = TempDir::new("codex-alloc-thread-day");
    let file = |day: &str| home.join(format!("sessions/{day}/rollout-2026-09-16T01-16-18-{thread}.jsonl"));
    let near = file("2026/09/16");
    fs::create_dir_all(near.parent().unwrap()).unwrap();
    fs::write(&near, "{}\n").unwrap();
    assert_eq!(rollout_of_thread(home.path(), thread), Some(near.clone()));
    fs::remove_file(&near).unwrap();
    let far = file("2026/09/18");
    fs::create_dir_all(far.parent().unwrap()).unwrap();
    fs::write(&far, "{}\n").unwrap();
    assert_eq!(rollout_of_thread(home.path(), thread), None);
}

/// A thread id comes from another process: one that is not all hex is
/// no id, whatever its length in bytes.
#[test]
fn codex_alloc_thread_id_not_all_hex_is_no_id() {
    assert_eq!("01a0a5da-da3c-7ééééééééé0".replace('-', "").len(), 32);
    assert_eq!(made_at("01a0a5da-da3c-7ééééééééé0"), None, "32 bytes, not 32 digits");
    assert_eq!(made_at("01a0a5da-da3c-7zzz-zzzz-zzzzzzzzzzzz"), None);
}

/// A pane gone for a scan and back reads its tail again; the refusal in
/// it is not written again, nor after a restart that read its going.
#[test]
fn codex_alloc_refusal_survives_a_gone_pane_and_a_restart() {
    let w = world("codex-alloc-refused-gone");
    let mut a = CodexAlloc::new(w.dir.clone());
    a.observe(&snap(&w, NOW, false));
    append(&w.rollout, &[refused_line(NOW + 5, 1)]);
    a.observe(&snap(&w, NOW + 30, false));
    assert_eq!(rows(&w.dir, "refused").len(), 1);
    a.observe(&Snapshot { panes: Vec::new(), ..snap(&w, NOW + 60, false) });
    a.observe(&snap(&w, NOW + 120, false));
    assert_eq!(rows(&w.dir, "refused").len(), 1, "back after a scan away");
    a.observe(&Snapshot { panes: Vec::new(), ..snap(&w, NOW + 180, false) });
    CodexAlloc::new(w.dir.clone()).observe(&snap(&w, NOW + 240, false));
    assert_eq!(rows(&w.dir, "refused").len(), 1, "nor after a restart");
}

/// A report of a turn older than the last refusal is an old one, though
/// its id is no longer among those kept.
#[test]
fn codex_alloc_refusal_older_than_the_last_is_not_new() {
    let w = world("codex-alloc-refused-old");
    let mut a = CodexAlloc::new(w.dir.clone());
    a.observe(&snap(&w, NOW, false));
    let lines: Vec<String> = (1..=17).map(|n| refused_line(NOW + 5 * i64::from(n), n)).collect();
    append(&w.rollout, &lines);
    a.observe(&snap(&w, NOW + 100, false));
    assert_eq!(rows(&w.dir, "refused").len(), 17);
    let mut s = snap(&w, NOW + 160, false);
    s.panes[0].peer.refused = Some((NOW + 5, Some(turn(1))));
    a.observe(&s);
    assert_eq!(rows(&w.dir, "refused").len(), 17, "turn 1 heard late from the peerbus");
}
