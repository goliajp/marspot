//! What the helper panel shows under each helper's name: its agent, its
//! model and the first line of its task.  Derived from the table once
//! per change of it.
//!
//! A helper's title covers its plugin's only while it is a shadow.  The
//! host tells the shell a plugin's title once, not on every scan, so
//! what each plugin last said is kept here to go back to.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::table::{Helper, Table};
use super::SHARED;
use crate::pane_decls::PaneDecls;

type Titles = Arc<Vec<(u64, String)>>;

/// The table the titles were last made from, and the titles.
static LAST: Mutex<Option<(Arc<Table>, Titles)>> = Mutex::new(None);
/// What each pane's plugin last titled it; one gone from the shell's
/// titles is dropped.
static PLUGINS: Mutex<Option<HashMap<u64, String>>> = Mutex::new(None);

/// Title each pane as its plugin said in `said`, and each helper in the
/// shadows by its task, whatever its agent's plugin says.
pub(crate) fn declare(decls: &mut PaneDecls<String>, said: impl Iterator<Item = (u64, String)>) {
    let mut plugins = PLUGINS.lock().unwrap_or_else(|p| p.into_inner());
    let plugins = plugins.get_or_insert_with(HashMap::new);
    plugins.retain(|sid, _| decls.declared(*sid).is_some());
    take_said(decls, plugins, said);
    let Some(titles) = titles().filter(|t| !t.is_empty()) else { return };
    let obs = crate::observe::shared().latest();
    declare_in(decls, plugins, &titles, |sid| obs.session(sid).is_some_and(|s| s.entry.shadow_of != 0));
}

/// What pane `sid`'s plugin last titled it.
pub(super) fn plugin_title(sid: u64) -> Option<String> {
    PLUGINS.lock().unwrap_or_else(|p| p.into_inner()).as_ref()?.get(&sid).cloned()
}

fn take_said(decls: &mut PaneDecls<String>, plugins: &mut HashMap<u64, String>, said: impl Iterator<Item = (u64, String)>) {
    for (sid, text) in said {
        decls.declare_ref(sid, &text);
        plugins.insert(sid, text);
    }
}

/// One in a window goes by its agent's plugin, as any pane there.
fn declare_in(
    decls: &mut PaneDecls<String>,
    plugins: &HashMap<u64, String>,
    titles: &[(u64, String)],
    is_shadow: impl Fn(u64) -> bool,
) {
    for (sid, title) in titles {
        if is_shadow(*sid) {
            decls.declare_ref(*sid, title);
        } else if decls.declared(*sid) == Some(title) {
            decls.declare(*sid, plugins.get(sid).cloned().unwrap_or_default());
        }
    }
}

/// Each helper's title.  None while there has been no helpers table.
fn titles() -> Option<Titles> {
    let table = SHARED.get()?.table();
    let mut last = LAST.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((of, titles)) = last.as_ref()
        && Arc::ptr_eq(of, &table)
    {
        return Some(Arc::clone(titles));
    }
    let titles: Titles = Arc::new(table.map.values().map(|h| (h.sid, title(h))).collect());
    *last = Some((table, Arc::clone(&titles)));
    Some(titles)
}

fn title(h: &Helper) -> String {
    let s = &h.started;
    let mut out = s.agent.as_str().to_string();
    if s.model != "default" {
        out.push(' ');
        out.push_str(&s.model);
    }
    if !s.task_line.is_empty() {
        out.push_str(" · ");
        out.push_str(&s.task_line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::table::Started;
    use super::*;
    use marspot_peer::control::{AgentKind, Isolation};

    #[test]
    fn helper_panel_titles_shadow_panes_alone() {
        let mut d = PaneDecls::new();
        let mut plugins = HashMap::new();
        let titles = [(7, "codex · a".to_string()), (8, "codex · b".to_string())];
        take_said(&mut d, &mut plugins, [(8, "spg".to_string())].into_iter());
        declare_in(&mut d, &plugins, &titles, |sid| sid == 7);
        assert_eq!(d.declared(7), Some(&titles[0].1));
        assert_eq!(d.declared(8).map(String::as_str), Some("spg"), "in a window: its plugin's");
        // moved to a window
        declare_in(&mut d, &plugins, &titles, |_| false);
        assert_eq!(d.declared(7).map(String::as_str), Some(""));
        assert_eq!(d.declared(8).map(String::as_str), Some("spg"));
    }

    /// The host tells the shell a plugin's title once: a helper moved to
    /// a window gets back what its plugin said while it was a shadow.
    #[test]
    fn helper_panel_title_moved_to_a_window_is_its_plugins_again() {
        let mut d = PaneDecls::new();
        let mut plugins = HashMap::new();
        let titles = [(7, "codex · a".to_string())];
        take_said(&mut d, &mut plugins, [(7, "spg".to_string())].into_iter());
        declare_in(&mut d, &plugins, &titles, |_| true);
        assert_eq!(d.declared(7), Some(&titles[0].1), "a shadow: its task");
        // later turns, the plugin saying nothing new
        declare_in(&mut d, &plugins, &titles, |_| true);
        take_said(&mut d, &mut plugins, std::iter::empty());
        declare_in(&mut d, &plugins, &titles, |_| false);
        assert_eq!(d.declared(7).map(String::as_str), Some("spg"));
        declare_in(&mut d, &plugins, &titles, |_| false);
        assert_eq!(d.declared(7).map(String::as_str), Some("spg"), "and stays so");
    }

    #[test]
    fn helper_panel_title_names_agent_model_and_task() {
        let mut h = Helper::new(
            7,
            Started {
                parent: 1,
                caller: "claude".into(),
                agent: AgentKind::Codex,
                account: "P1".into(),
                model: "default".into(),
                mode: "auto".into(),
                isolation: Isolation::Host,
                budget_min: 60,
                task_line: "review the parser".into(),
                task_sha: String::new(),
                label: String::new(),
                title: String::new(),
                conv: None,
                root: None,
            },
            0,
        );
        assert_eq!(title(&h), "codex · review the parser");
        h.started.model = "gpt-6".into();
        assert_eq!(title(&h), "codex gpt-6 · review the parser");
        h.started.task_line.clear();
        assert_eq!(title(&h), "codex gpt-6");
    }
}
