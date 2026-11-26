use marspot::zombie::probe::Live;
use marspot_term::submit_receipt::{ensure_secret, token_for};

use super::*;
use crate::observe::tests::row;
use crate::plugins::zombie::attrib::tests::{Real, known, observe};

const NOW: u64 = 1_800_000_000;

fn scene<'a>(table: &'a marspot::pidtree::ProcTable, outside: Vec<OutsideIn<'a>>) -> Scene<'a> {
    Scene { now: NOW, table, panes: Vec::new(), outside, uid: 501, tmpdir: None, helper: &|_, _| false }
}

#[test]
fn zombie_orphans_names_remember_a_week_of_panes_and_at_most_their_cap() {
    let mut n = Names::default();
    n.note([(7, "spg".to_string())], &[(12, 7)], NOW);
    assert_eq!(n.name(7), "spg");
    assert_eq!(n.parents(), [(12, 7)].into());
    n.note([(8, "alt".to_string())], &[], NOW + NAMES_FOR - 1);
    assert_eq!(n.name(7), "spg", "seen a moment less than a week ago");
    n.note([], &[], NOW + NAMES_FOR);
    assert_eq!((n.name(7), n.name(8)), ("", "alt"), "a week after it was last seen it is forgotten");
    let many = (0..NAMES_MAX as u64 + 20).map(|sid| (100 + sid, format!("p{sid}")));
    n.note(many, &[], NOW + NAMES_FOR + 1);
    assert_eq!(n.rows.len(), NAMES_MAX);

    let dir = std::env::temp_dir().join(format!("zombie-names-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut m = Names::default();
    m.note([(7, "spg\tx".to_string())], &[(12, 7)], NOW);
    std::fs::write(dir.join("names.tsv"), m.tsv()).unwrap();
    let back = Names::load(&dir);
    assert_eq!((back.name(7), back.parents()), ("spg x", [(12, 7)].into()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn zombie_orphans_are_the_roots_a_closed_panes_token_names() {
    ensure_secret().expect("a secret");
    let closed_token = token_for(12).unwrap();
    let live_token = token_for(9).unwrap();
    let (left, _) = Real::sleeping(&[("MARSPOT_SUBMIT_TOKEN", &closed_token)]);
    let (kept, _) = Real::sleeping(&[("MARSPOT_SUBMIT_TOKEN", &live_token)]);
    let root = left.row(1);
    let child = row(99_301, root.pid, "node");
    let mut ours = row(99_302, 1, "marspot-session");
    ours.start_us = root.start_us;
    let obs = observe(vec![root.clone(), child, kept.row(1), ours]);
    let in_pane: HashSet<i32> = [2, 3, 4, 5].into();
    let mut a = Attributor::default();
    let k = known(None);
    a.begin(&obs, &k);
    let (found, later) = closed(&obs, &in_pane, &k, &mut a);
    let ids: Vec<(i32, u64)> = found.iter().map(|o| (o.row.pid, o.sid)).collect();
    assert_eq!(ids, vec![(root.pid, 12)], "only the root, not its child, the live pane's process or marspot's own");
    assert!(later.is_empty());

    let mut names = Names::default();
    names.note([(12, "spg".to_string())], &[], NOW - 60);
    let s = scene(&obs.table, found);
    let rows = record(&[], &s.outside, &later, &s, &mut Watch::default(), &mut Live::default(), &names);
    assert_eq!(rows.len(), 1);
    let o = &rows[0];
    assert_eq!((o.id, o.sid, o.name.as_str(), o.first_seen), (root.id(), 12, "spg", NOW));
    assert!(o.rules.is_empty(), "a sleeping test binary meets neither O1 nor O2");
    assert!(!o.argv.is_empty());
}

#[test]
fn zombie_orphans_record_keeps_first_seen_and_a_waiting_line_while_its_process_lives() {
    ensure_secret().expect("a secret");
    let token = token_for(12).unwrap();
    let (left, _) = Real::sleeping(&[("MARSPOT_SUBMIT_TOKEN", &token)]);
    let (waiting, _) = Real::sleeping(&[]);
    let r = left.row(1);
    let obs = observe(vec![r.clone(), waiting.row(1)]);
    let found = vec![OutsideIn { sid: 12, row: obs.table.get(r.pid).unwrap(), group: Vec::new(), session_gone: false, why: String::new() }];
    let line = |id: ProcId, first_seen: u64| Orphan { first_seen, id, sid: 12, name: String::new(), rules: String::new(), argv: "x".into() };
    let gone = ProcId { pid: 99_400, start_us: 1 };
    let before = vec![line(r.id(), NOW - 600), line(waiting.row(1).id(), NOW - 300), line(gone, NOW - 300)];
    let later: HashSet<ProcId> = [waiting.row(1).id(), gone].into();
    let s = scene(&obs.table, found);
    let rows = record(&before, &s.outside, &later, &s, &mut Watch::default(), &mut Live::default(), &Names::default());
    let got: Vec<(ProcId, u64)> = rows.iter().map(|o| (o.id, o.first_seen)).collect();
    assert_eq!(got, vec![(r.id(), NOW - 600), (waiting.row(1).id(), NOW - 300)], "the line of a process that is gone is dropped");
}
