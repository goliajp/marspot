//! What each pane of a window is called on screen, in its title and in
//! the sidebar.

use std::collections::HashMap;

use super::*;

impl CoreApp {
    /// What each pane of the window is called on screen, given every
    /// pane's name (`pane_names`).
    pub(super) fn pane_labels(&self, wi: usize, pane_names: &HashMap<u64, String>) -> Vec<String> {
        // Pane numbers from the static table: this used to be a `Vec`
        // and one `String` per pane, rebuilt every frame for text that
        // never changes.
        let labels = &marspot::ui::PANE_NUMBER_LABELS[..win!(self, wi)
            .panes
            .len()
            .min(marspot::ui::PANE_NUMBER_LABELS.len())];
        // Title placeholder = basename of the cwd cached in
        // `pane_cwds`.  Pure read: `sweep_pane_cwds` owns population
        // (once per CWD_SWEEP_INTERVAL from the main loop's periodic
        // block, plus a one-shot resolve at pane spawn and on
        // LayoutModal open).  Nothing here may issue a syscall — this
        // runs per frame, per window.
        let cwd_basenames: Vec<Option<&str>> = (0..win!(self, wi).panes.len())
            .map(|i| {
                let sid = win!(self, wi).panes[i].shelld_session_id()?;
                let path = self.pane_cwds.get(&sid)?;
                std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
            })
            .collect();

        // Resolved label per cell: the pane's own name (its directory,
        // with its code when shared — `marspot::pane_name`) → plugin-set
        // title (MsgType::PaneTitle, cc/...) → ordinal fallback.
        //
        // There is no user-set title any more: a pane's name is derived,
        // never stored, so it cannot disagree with what `--send` will
        // accept.  A rename would have made the name a second source of
        // truth about which pane is which.
        (0..win!(self, wi).panes.len())
            .map(|i| {
                if let Some(name) = win!(self, wi).panes[i]
                    .shelld_session_id()
                    .and_then(|sid| pane_names.get(&sid))
                    .filter(|n| !n.is_empty() && *n != "?")
                {
                    name.clone()
                } else if let Some(plugin_title) = win!(self, wi).panes[i]
                    .shelld_session_id()
                    .and_then(|sid| self.pane_titles.get(&sid))
                {
                    plugin_title.clone()
                } else if let Some(Some(name)) = cwd_basenames.get(i) {
                    (*name).to_string()
                } else {
                    labels.get(i).map(|s| (*s).to_string()).unwrap_or_default()
                }
            })
            .collect()
    }

    /// The code of pane `i` of window `wi`, from memory only: drawing
    /// never waits on the code file (`note_pane_code` fills it).
    #[cfg(test)]
    pub(crate) fn pane_code(&self, wi: usize, i: usize) -> Option<u16> {
        marspot_term::pane_code::known(win!(self, wi).panes.get(i)?.shelld_session_id()?)
    }

    /// Each sidebar row's label, state and code.  The code is drawn
    /// before the name, so a shared name loses its own copy of it.
    /// `resolved` and `names` are what `pane_labels` and `pane_names`
    /// gave this frame.
    pub(super) fn sidebar_texts(
        &self,
        wi: usize,
        resolved: &[String],
        names: &HashMap<u64, String>,
    ) -> Vec<(String, SessionState, Option<u16>)> {
        use super::sidebar_tree::RowPane;
        let rows = self.sidebar_rows(wi);
        rows.iter()
            .map(|r| {
                let (label, pane) = match r.pane {
                    RowPane::Slot(i) => (resolved.get(i).map_or("", |s| s.as_str()), win!(self, wi).panes.get(i)),
                    RowPane::Other(sid) => (self.label_of(sid, names), self.pane_of(sid)),
                };
                let code = pane.and_then(Pane::shelld_session_id).and_then(marspot_term::pane_code::known);
                let s = code.map_or(label, |c| without_code(label, c));
                let state = pane.map_or(SessionState::Exited, |p| p.session().state());
                (truncate_for_sidebar(s, MAX_SIDEBAR_LABEL_CHARS), state, code)
            })
            .collect()
    }

    /// What a pane of no slot of this window is called on screen.
    fn label_of<'a>(&'a self, sid: u64, names: &'a HashMap<u64, String>) -> &'a str {
        let named = names.get(&sid).filter(|n| !n.is_empty() && *n != "?");
        let basename = || {
            let path = self.pane_cwds.get(&sid)?;
            std::path::Path::new(path).file_name()?.to_str()
        };
        named.or_else(|| self.pane_titles.get(&sid)).map(|s| s.as_str()).or_else(basename).unwrap_or("")
    }

    /// The pane whose session is `sid`, in a window or not.
    pub(super) fn pane_of(&self, sid: u64) -> Option<&Pane> {
        self.windows.iter().flat_map(|w| w.panes.iter()).chain(&self.shadows).find(|p| p.shelld_session_id() == Some(sid))
    }
}

/// `label` without a trailing `#<code>` of `code`.
pub(crate) fn without_code(label: &str, code: u16) -> &str {
    match label.rsplit_once('#') {
        Some((base, digits)) if !base.is_empty() && digits.parse::<u16>().ok() == Some(code) => base,
        _ => label,
    }
}

#[cfg(test)]
#[test]
fn sidebar_label_drops_only_its_own_code() {
    assert_eq!(without_code("spg#3381", 3381), "spg");
    assert_eq!(without_code("spg#0042", 42), "spg");
    assert_eq!(without_code("spg#3381", 3382), "spg#3381");
    assert_eq!(without_code("#3381", 3381), "#3381");
    assert_eq!(without_code("codex · a task", 3381), "codex · a task");
}
