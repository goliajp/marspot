use std::path::Path;
use std::time::{Duration, SystemTime};

use super::super::quota::{How, from_info, from_screen};
use super::super::session::{Asker, SessionIndex};
use super::super::fixture::*;
use super::*;
use crate::plugins::allocation::replay::{Why, replay};
use crate::plugins::handoff::history::tmp;
use crate::plugins::pidtree::ProcId;

/// 2026-11-25T10:00:00Z.
const NOW: i64 = 1_795_600_800;
const ID: &str = "11111111-2222-4333-8444-555555555555";
const PRO: &str = "gemini-3-pro-preview";

fn rows(dir: &Path) -> Vec<Event> {
    EventLog::for_agent(dir.to_path_buf(), "gemini").read_since(NOW - DAY, NOW + DAY)
}

fn pane(sid: u64, profile: u8) -> PaneIn {
    PaneIn { sid, profile, started_at: NOW - 3600, model: Some(PRO.into()), refused: None, on_screen: None, quiet: true, held: None, helper: false }
}

fn snap(panes: Vec<PaneIn>) -> Snapshot {
    Snapshot { now: NOW, homes: vec![1, 2, 3], panes }
}

fn said_of(cfg: &Path) -> super::super::session::Said {
    let who = Asker { proc: ProcId { pid: 7, start_us: 7 }, started: SystemTime::now() - Duration::from_secs(1), config_dir: cfg, cwd: "/w/a", session: Some(ID), lone: true };
    let mut idx = SessionIndex::default();
    idx.set_quota(true);
    idx.facts(&who).unwrap().said.unwrap()
}

#[test]
fn gemini_allocation_a_quota_error_in_the_session_file_is_a_refusal_until_the_model_answers() {
    let cfg = tmp("gemini-alloc-error");
    let file = project(&cfg, "/w/a", "a").join(file_name(ID));
    let daily = "[API Error: You have exhausted your daily quota on this model.]";
    std::fs::write(&file, head(ID) + &reply_at("g1", "2026-11-10T06:11:00.000Z", PRO) + &error_at("e1", "2026-11-10T06:12:00.000Z", daily)).unwrap();
    let r = said_of(&cfg).refused.expect("the error is a refusal");
    assert_eq!((r.scope, r.how, r.at), (Scope::Model(PRO.into()), How::Error, 1_794_291_120));

    // capacity is the server's, not the account's
    std::fs::write(&file, head(ID) + &error_at("e1", "2026-11-10T06:12:00.000Z", "[API Error: MODEL_CAPACITY_EXHAUSTED]")).unwrap();
    assert_eq!(said_of(&cfg).refused, None);

    // the same model answers later: the refusal is over
    let later = reply_at("g2", "2026-11-10T07:00:00.000Z", PRO);
    std::fs::write(&file, head(ID) + &reply_at("g1", "2026-11-10T06:11:00.000Z", PRO) + &error_at("e1", "2026-11-10T06:12:00.000Z", daily) + &later).unwrap();
    assert_eq!(said_of(&cfg).refused, None);
}

#[test]
fn gemini_allocation_a_fallback_switch_refuses_the_model_it_left() {
    let cfg = tmp("gemini-alloc-fallback");
    let file = project(&cfg, "/w/a", "a").join(file_name(ID));
    let body = head(ID)
        + &reply_at("g1", "2026-11-10T06:11:00.000Z", PRO)
        + &info_at("i1", "2026-11-10T06:12:00.000Z", "Switched to fallback model gemini-3-flash-preview")
        + &reply_at("g2", "2026-11-10T06:13:00.000Z", "gemini-3-flash-preview");
    std::fs::write(&file, body).unwrap();
    let r = said_of(&cfg).refused.expect("a fallback is a refusal of the model before it");
    assert_eq!((r.scope, r.how), (Scope::Model(PRO.into()), How::Fallback), "the fallback's own replies do not end it");

    // written once, and a restart reads it back instead of writing it again
    let dir = tmp("gemini-alloc-fallback-events");
    let mut p = pane(5, 1);
    p.refused = from_info("Switched to fallback model gemini-3-flash-preview", NOW - 60, Some(PRO));
    let mut a = GeminiAlloc::new(dir.clone());
    let (_, moves) = a.observe(&snap(vec![p.clone()]), |_, _| false);
    a.observe(&snap(vec![p.clone()]), |_, _| false);
    GeminiAlloc::new(dir.clone()).observe(&snap(vec![p.clone()]), |_, _| false);
    let refused: Vec<Event> = rows(&dir).into_iter().filter(|e| e.kind == "refused").collect();
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0].profile, Some(1));
    assert!(refused[0].detail.contains(&format!("model={PRO} how=fallback")), "{}", refused[0].detail);
    assert_eq!(moves.iter().map(|m| (m.sid, m.to)).collect::<Vec<_>>(), [(5, 2)], "the pane must move, to the first home with room");

    // a refusal from before this gemini started is another process's history
    let mut old = pane(6, 3);
    old.refused = from_info("Switched to fallback model x", NOW - 7200, Some(PRO));
    let (_, moves) = GeminiAlloc::new(tmp("gemini-alloc-old")).observe(&snap(vec![old]), |_, _| false);
    assert!(moves.is_empty());
}

#[test]
fn gemini_allocation_the_paid_dialogs_are_refusals_and_a_flash_pane_stays() {
    let dir = tmp("gemini-alloc-dialog");
    let overage = "│ Usage limit reached for gemini-3-pro-preview. │\n│ You have 120 AI Credits available. │\n│ ● 1. Use AI Credits - Continue this request (Overage) │";
    let empty = "│ Usage limit reached for gemini-3-pro-preview. │\n│ To continue using this model now, purchase more AI Credits. │\n│ ● 1. Get AI Credits - Open browser to purchase credits │";
    let mut on_p1 = pane(1, 1);
    on_p1.on_screen = from_screen(overage, NOW);
    let mut on_p2 = pane(2, 2);
    on_p2.on_screen = from_screen(empty, NOW);
    let mut flash = pane(3, 1);
    flash.model = Some("gemini-3-flash-preview".into());
    let (_, moves) = GeminiAlloc::new(dir.clone()).observe(&snap(vec![on_p1, on_p2, flash]), |_, _| true);
    let how: Vec<String> = rows(&dir).into_iter().filter(|e| e.kind == "refused").map(|e| e.detail.split(' ').find(|w| w.starts_with("how=")).unwrap().to_string()).collect();
    assert_eq!(how, ["how=overage", "how=empty_wallet"]);
    assert_eq!(moves.iter().map(|m| (m.sid, m.to)).collect::<Vec<_>>(), [(1, 3), (2, 3)], "both to the one home left; the flash pane is not refused");
}

#[test]
fn gemini_allocation_a_would_move_replays_to_the_same_target_and_names_the_path() {
    let dir = tmp("gemini-alloc-replay");
    let mut p = pane(9, 1);
    p.refused = from_screen("Usage limit reached for all Pro models.\nAccess resets at 4:30 PM PST.\n● 1. Switch to gemini-3-flash-preview\n  2. Stop", NOW);
    let mut busy = pane(10, 2);
    busy.quiet = false;
    busy.refused = p.refused.clone();
    let mut a = GeminiAlloc::new(dir.clone());
    let (_, moves) = a.observe(&snap(vec![p.clone(), busy.clone()]), |_, to| to == 3);
    assert_eq!(moves.len(), 1, "a pane still writing is not moved: {moves:?}");
    assert!(moves[0].resume);
    let (_, moves) = a.observe(&snap(vec![p, busy]), |_, _| false);
    assert!(!moves[0].resume, "a target without the session is handed the conversation");
    let would: Vec<Event> = rows(&dir).into_iter().filter(|e| e.kind == "would_move").collect();
    assert_eq!(would.len(), 1, "an unchanged target is written once");
    assert!(would[0].detail.starts_with("to=P3 why=out via=resume "), "{}", would[0].detail);
    assert_eq!(replay(&would[0].detail), Some(Some((3, Why::Out))), "P1 and P2 refused Pro, so P3");
}

#[test]
fn gemini_allocation_every_home_refused_means_staying_and_a_refusal_ends_with_its_day() {
    let mut p = pane(4, 1);
    p.refused = from_info("Switched to fallback model f", NOW - 60, Some(PRO));
    let others: Vec<PaneIn> = [2, 3]
        .into_iter()
        .map(|h| PaneIn { sid: u64::from(h) + 10, profile: h, refused: from_info("Switched to fallback model f", NOW - 30, Some(PRO)), ..pane(0, h) })
        .collect();
    let mut a = GeminiAlloc::new(tmp("gemini-alloc-all-out"));
    let mut all = vec![p.clone()];
    all.extend(others);
    let (log, moves) = a.observe(&snap(all), |_, _| false);
    assert!(moves.is_empty());
    assert_eq!(log.iter().filter(|(t, _)| *t == "gemini.alloc.notice").count(), 3);
    let next_day = Snapshot { now: pacific_midnight_after(NOW) + 1, homes: vec![1, 2, 3], panes: vec![p] };
    let (_, moves) = a.observe(&next_day, |_, _| false);
    assert!(moves.is_empty() && a.refused.is_empty(), "Pacific midnight renews the day");
}

#[test]
fn gemini_allocation_off_reads_no_refusal_from_the_session_file() {
    let cfg = tmp("gemini-alloc-off-said");
    let file = project(&cfg, "/w/a", "a").join(file_name(ID));
    let daily = "[API Error: You have exhausted your daily quota on this model.]";
    let body = head(ID)
        + &reply_at("g1", "2026-11-10T06:11:00.000Z", PRO)
        + &error_at("e1", "2026-11-10T06:12:00.000Z", daily)
        + &info_at("i1", "2026-11-10T06:13:00.000Z", "Switched to fallback model gemini-3-flash-preview");
    std::fs::write(&file, body).unwrap();
    let who = Asker { proc: ProcId { pid: 7, start_us: 7 }, started: SystemTime::now() - Duration::from_secs(1), config_dir: &cfg, cwd: "/w/a", session: Some(ID), lone: true };
    let said = SessionIndex::default().facts(&who).unwrap().said.unwrap();
    assert_eq!(said.refused, None, "off parses no refusal");
    assert_eq!(said.fallback.as_deref(), Some("gemini-3-flash-preview"), "the badge still follows the fallback");
}

#[test]
fn gemini_allocation_a_boxed_dialog_s_refusal_reads_back_after_a_restart() {
    let dir = tmp("gemini-alloc-roundtrip");
    let screen = "│ Usage limit reached for gemini-3.1-pro- │\n│ preview.                                │\n│ ● 1. Stop                               │";
    let mut p = pane(1, 1);
    p.model = Some("gemini-3.1-pro-preview".into());
    p.on_screen = from_screen(screen, NOW - 30);
    GeminiAlloc::new(dir.clone()).observe(&snap(vec![p.clone()]), |_, _| false);
    let mut again = GeminiAlloc::new(dir.clone());
    again.observe(&snap(vec![p]), |_, _| false);
    let refused: Vec<Event> = rows(&dir).into_iter().filter(|e| e.kind == "refused").collect();
    assert_eq!(refused.len(), 1, "read back, not written again: {refused:?}");
    assert!(again.refused.contains_key(&(1, Scope::Model("gemini-3.1-pro-preview".into()))));
}
