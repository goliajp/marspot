//! The title a Codex process has run under for hours, as a source of
//! its thread when nothing shows the process wrote that title itself.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use super::bind::{Pane, TITLE_SETTLED_MS, Unbound};

/// Bind each pane not yet decided whose configuration shows the thread
/// in the title and that has run under its title for hours to the
/// thread that title names.  A TUI that shows its thread rewrites the
/// title as it draws, so one that still names an older thread by then
/// is showing that thread, however many other Codex processes have been
/// in its directory meanwhile.
///
/// The title may still be one an earlier process left in the pane, so
/// it never takes a thread another pane is known to run: one in `taken`
/// (a title that process wrote), one another undecided pane is bound to
/// in `current` while it is still loaded in that pane's directory, or
/// one another pane's command line resumes.  Two panes whose settled
/// titles name the same thread both wait: one of them is a stale title,
/// and which one is not said.
pub(super) fn settled_titles(
    panes: &[Pane],
    threads: &HashMap<PathBuf, Vec<(String, String)>>,
    current: &HashMap<u64, String>,
    out: &mut BTreeMap<u64, Result<String, Unbound>>,
    taken: &mut Vec<(PathBuf, String)>,
) {
    let loaded_in = |home: &PathBuf, t: &str, cwd: &str| {
        threads.get(home).is_some_and(|ts| ts.iter().any(|(id, c)| id == t && c == cwd))
    };
    let runs_elsewhere = |p: &Pane, t: &str| {
        taken.iter().any(|(h, x)| *h == p.home && x == t)
            || panes.iter().any(|q| {
                q.sid != p.sid
                    && q.home == p.home
                    && !out.contains_key(&q.sid)
                    && (q.argv_thread.as_deref() == Some(t)
                        || current.get(&q.sid).is_some_and(|c| c == t && loaded_in(&q.home, t, &q.cwd)))
            })
    };
    let mut found: Vec<(u64, PathBuf, Result<String, Unbound>)> = Vec::new();
    for p in panes.iter().filter(|p| !out.contains_key(&p.sid)) {
        let Some(prefix) = &p.title_thread else { continue };
        if !p.title_expected || p.up_ms < TITLE_SETTLED_MS {
            continue;
        }
        let named: Vec<&str> = threads.get(&p.home).map_or(Vec::new(), |ts| {
            ts.iter().map(|(id, _)| id.as_str()).filter(|id| id.starts_with(prefix.as_str())).collect()
        });
        let r = match named.as_slice() {
            [] => Err(Unbound::NoThread),
            [one] if runs_elsewhere(p, one) => Err(Unbound::AwaitingTitle),
            [one] => Ok(one.to_string()),
            many => Err(Unbound::Ambiguous { panes: 1, threads: many.len() }),
        };
        found.push((p.sid, p.home.clone(), r));
    }
    let twice =
        |home: &PathBuf, t: &str| found.iter().filter(|(_, h, r)| h == home && r.as_ref().is_ok_and(|x| x == t)).count() > 1;
    let decided: Vec<(u64, PathBuf, Result<String, Unbound>)> = found
        .iter()
        .map(|(sid, h, r)| match r {
            Ok(t) if twice(h, t) => (*sid, h.clone(), Err(Unbound::AwaitingTitle)),
            _ => (*sid, h.clone(), r.clone()),
        })
        .collect();
    for (sid, home, r) in decided {
        if let Ok(t) = &r {
            taken.push((home, t.clone()));
        }
        out.insert(sid, r);
    }
}
