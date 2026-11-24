//! What the helper panel shows under each helper's name: its agent, its
//! model and the first line of its task.  Derived from the table once
//! per change of it.

use std::sync::{Arc, Mutex};

use super::table::{Helper, Table};
use super::SHARED;
use crate::pane_decls::PaneDecls;

type Titles = Arc<Vec<(u64, String)>>;

/// The table the titles were last made from, and the titles.
static LAST: Mutex<Option<(Arc<Table>, Titles)>> = Mutex::new(None);

/// Title each helper in the shadows by its task, whatever its agent's
/// plugin says.
pub(crate) fn declare(decls: &mut PaneDecls<String>) {
    let Some(titles) = titles().filter(|t| !t.is_empty()) else { return };
    let obs = crate::observe::shared().latest();
    declare_in(decls, &titles, |sid| obs.session(sid).is_some_and(|s| s.entry.shadow_of != 0));
}

/// One in a window goes by its agent's plugin, as any pane there.
fn declare_in(decls: &mut PaneDecls<String>, titles: &[(u64, String)], is_shadow: impl Fn(u64) -> bool) {
    for (sid, title) in titles {
        if is_shadow(*sid) {
            decls.declare_ref(*sid, title);
        } else if decls.declared(*sid) == Some(title) {
            decls.declare(*sid, String::new());
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
        let titles = [(7, "codex · a".to_string()), (8, "codex · b".to_string())];
        d.declare(8, "spg".to_string());
        declare_in(&mut d, &titles, |sid| sid == 7);
        assert_eq!(d.declared(7), Some(&titles[0].1));
        assert_eq!(d.declared(8).map(String::as_str), Some("spg"), "in a window: its plugin's");
        // moved to a window
        declare_in(&mut d, &titles, |_| false);
        assert_eq!(d.declared(7).map(String::as_str), Some(""));
        assert_eq!(d.declared(8).map(String::as_str), Some("spg"));
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
