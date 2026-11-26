//! Which conversation each claude pane holds, for other plugins: the
//! zombie watch tells a process outside every pane by the session id it
//! inherited.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::bind_meta::BindMeta;
use crate::host::published::Published;
use super::argv::is_uuid;
use super::{config_dir_or_default, continued, encode_project_dir};

/// A claude pane's conversation, as the last scan bound it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClaudePane {
    pub sid: u64,
    pub uuid: String,
    /// The project directory its transcripts are in, when known.
    pub transcripts: Option<PathBuf>,
}

static PANES: Published<Vec<ClaudePane>> = Published::new();

/// Every bound claude pane's conversation.
pub(crate) fn sessions() -> Arc<Vec<ClaudePane>> {
    PANES.get().unwrap_or_default()
}

pub(super) fn publish(meta: &HashMap<u64, BindMeta>) {
    let mut panes: Vec<ClaudePane> = meta
        .iter()
        .filter(|(_, m)| !m.uuid.is_empty())
        .map(|(sid, m)| ClaudePane {
            sid: *sid,
            uuid: m.uuid.clone(),
            transcripts: m.project_dir.as_deref().map(|p| {
                config_dir_or_default(m.config_dir.as_deref()).join("projects").join(encode_project_dir(Path::new(p)))
            }),
        })
        .collect();
    panes.sort_by_key(|p| p.sid);
    PANES.publish(panes);
}

/// The pane whose conversation is `session`, or the one `session`'s
/// conversation was carried on in.  `session` comes from another
/// process's environment, so it is used as a file name only when it
/// has the shape of a session id.
pub(crate) fn pane_of_session(panes: &[ClaudePane], session: &str) -> Option<u64> {
    if let Some(p) = panes.iter().find(|p| p.uuid == session) {
        return Some(p.sid);
    }
    if !is_uuid(session) {
        return None;
    }
    panes.iter().find_map(|p| {
        let dir = p.transcripts.as_deref()?;
        let carried = dir.join(format!("{session}.jsonl")).is_file() && continued::latest(dir, session) == p.uuid;
        carried.then_some(p.sid)
    })
}
