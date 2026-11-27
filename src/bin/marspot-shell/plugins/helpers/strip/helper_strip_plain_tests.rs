//! Shadow panes no helper runs in are listed in the strip of their top
//! pane, as helpers are, with or without helpers beside them.

use std::sync::Arc;
use std::time::Instant;

use marspot::pane_state::{BusyKind, PaneStatus};
use marspot::pidtree::ProcTable;
use marspot::shell_proto::{HelperStrip, StripAgent, StripState};
use marspot_peer::control::AgentKind;

use super::super::helper_strip_tests::{begin, dir_gone, number, panes};
use super::super::{Memory, declare_in};
use super::Panes;
use crate::observe::Observation;
use crate::observe::tests::{entry, row};
use crate::pane_decls::PaneDecls;
use crate::plugins::helpers::tests::{shared_at, temp};

/// Pane 10 at its shell, and pane 11, a shadow of it with `front` in
/// front: a `pane.open` under 10 that runs `front`.
fn with_shadow(front: &str) -> Arc<Observation> {
    let mut sh = row(51, 1, "zsh");
    sh.tty_fg_pgid = 61;
    let mut prog = row(61, 51, front);
    (prog.pgid, prog.tty_fg_pgid) = (61, 61);
    let mut shadow = entry(11, 2, 51);
    shadow.shadow_of = 10;
    shadow.cwd = "/work/madpdf".into();
    let rows = vec![row(50, 1, "zsh"), sh, prog];
    let argv = |r: &marspot::pidtree::ProcRow| Some(Arc::from([r.comm.clone()]));
    Arc::new(Observation::from_parts(Instant::now(), ProcTable::from_rows(rows), vec![(entry(10, 1, 50), true), (shadow, true)], argv))
}

fn busy(_: u64) -> Option<PaneStatus> {
    Some(PaneStatus::Busy(BusyKind::Output))
}

fn untitled(_: u64) -> Option<String> {
    None
}

const BUSY: Panes<'static> = Panes { status: &busy, title: &untitled };

/// What the pass sends: each root and its record.
fn sent(decls: &mut PaneDecls<HelperStrip>) -> Vec<HelperStrip> {
    let mut out = Vec::new();
    decls.flush(|_, x| {
        out.push(x.clone());
        true
    });
    out
}

#[test]
fn helper_strip_plain_shadow_under_a_pane_with_no_helpers_is_listed() {
    let d = temp("strip-plain");
    let (s, _rx) = shared_at(&d);
    let (mut decls, mut m) = (PaneDecls::new(), Memory::default());
    let obs = with_shadow("claude");
    declare_in(&mut decls, &s, &mut m, &|| Arc::clone(&obs), &number, &BUSY);
    let got = sent(&mut decls);
    assert_eq!(got.len(), 1);
    let (x, r) = (&got[0], &got[0].rows[0]);
    assert_eq!((x.root_sid, x.rows.len()), (10, 1), "one row, under its top pane");
    assert!(x.batch > 0, "a record with rows has a batch");
    assert_eq!((r.sid, r.number, r.plain, r.gone), (11, Some(1011), true, false));
    assert_eq!((r.agent, r.name.as_str(), r.state), (StripAgent::Claude, "claude", StripState::Working));
    assert_eq!(r.task, "madpdf", "untitled: its directory");
    assert_eq!((r.worked_ms, r.running_since_ms, r.tokens), (0, None, None), "nothing times it");
    // titled by its plugin, and at a program that is no agent
    let titled = |_: u64| Some("fix the page breaks".to_string());
    let named = Panes { status: &busy, title: &titled };
    let vim = with_shadow("vim");
    declare_in(&mut decls, &s, &mut m, &|| Arc::clone(&vim), &number, &named);
    let r = sent(&mut decls)[0].rows[0].clone();
    assert_eq!((r.agent, r.name.as_str(), r.task.as_str()), (StripAgent::Other, "vim", "fix the page breaks"));
    // closed: the pane is told it has none
    declare_in(&mut decls, &s, &mut m, &|| panes(&[(10, 0)]), &number, &BUSY);
    assert_eq!(sent(&mut decls), [HelperStrip::empty(10)]);
    dir_gone(&d);
}

#[test]
fn helper_strip_plain_shadow_joins_the_helpers_of_its_top_pane() {
    let d = temp("strip-plain-mixed");
    let (s, _rx) = shared_at(&d);
    let (mut decls, mut m) = (PaneDecls::new(), Memory::default());
    begin(&s, 1, 10, AgentKind::Codex);
    // 12 under the helper 1, 13 under 12: all three under 10
    let obs = panes(&[(10, 0), (1, 10), (12, 1), (13, 12)]);
    declare_in(&mut decls, &s, &mut m, &|| Arc::clone(&obs), &number, &BUSY);
    let got = sent(&mut decls);
    assert_eq!(got.len(), 1);
    let rows: Vec<(u64, bool)> = got[0].rows.iter().map(|r| (r.sid, r.plain)).collect();
    assert_eq!(rows.len(), 3);
    assert!(rows.contains(&(1, false)) && rows.contains(&(12, true)) && rows.contains(&(13, true)));
    let helpers_batch = s.lock().batches.open()[&10].number;
    assert_eq!(got[0].batch, helpers_batch, "the helpers' batch, not one of its own");
    dir_gone(&d);
}
