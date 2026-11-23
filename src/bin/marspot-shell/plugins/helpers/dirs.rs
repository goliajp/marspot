//! The helpers' directories (task, script, result): one goes when its
//! helper ends, with any an earlier end left behind.

use std::cell::OnceCell;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Shared, container, records};

/// Remove `sid`'s directory, if there is one.
pub(super) fn remove(root: &Path, sid: u64) {
    match std::fs::remove_dir_all(root.join(sid.to_string())) {
        Err(e) if e.kind() != ErrorKind::NotFound => marspot::lx_warn!("helpers.dir_left", &format!("sid={sid}: {e}")),
        _ => {}
    }
}

impl Shared {
    pub(super) fn root(&self) -> PathBuf {
        self.log.parent().map_or_else(records::dir, PathBuf::from)
    }

    /// The directory a helper's task, script and result live in.
    pub(super) fn dir_of(&self, sid: u64) -> PathBuf {
        self.root().join(sid.to_string())
    }

    /// Whether `sid`'s directory may go: the log or this run says the
    /// helper stopped, or its session is `gone`, and no container of it
    /// may still write there.  Not being in the table
    /// is no reason, as the table may lack a helper that runs: one whose
    /// line could not be written, or not be read back.
    pub(super) fn dir_is_over(&self, sid: u64, gone: impl FnOnce(u64) -> bool) -> bool {
        let st = self.lock();
        if st.table.map.contains_key(&sid) {
            return false;
        }
        let stopped = st.stopped.contains(&sid);
        drop(st);
        (stopped || gone(sid)) && !container::pending(&self.root(), sid)
    }

    /// Remove the directories that are over, the one of the helper that
    /// just ended among them.  An agent still on its way out can write
    /// into its directory while it is removed, or make it again after:
    /// what that leaves goes at the next end or the next start.
    pub(super) fn sweep_dirs(&self) {
        let epoch = self.epoch();
        // walked once the directories are listed: a session is there
        // before its directory is
        let walk = OnceCell::new();
        self.sweep_dirs_where(|sid| {
            let obs = walk.get_or_init(|| crate::observe::shared().fresh(Duration::ZERO));
            // the observer goes after a park, and shows no session then
            obs.session(sid).is_none() && self.runs(epoch)
        });
    }

    /// [`Shared::sweep_dirs`], with `gone` saying whose session is gone.
    pub(super) fn sweep_dirs_where(&self, gone: impl Fn(u64) -> bool) {
        let Ok(rd) = std::fs::read_dir(self.root()) else { return };
        let found: Vec<u64> = rd.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect();
        for sid in found.iter().copied().filter(|sid| self.dir_is_over(*sid, &gone)) {
            remove(&self.root(), sid);
        }
        // no directory and no session to make one: nothing to remember
        let stopped: Vec<u64> = self.lock().stopped.iter().copied().filter(|sid| !found.contains(sid)).collect();
        let done: Vec<u64> = stopped.into_iter().filter(|sid| gone(*sid)).collect();
        let mut st = self.lock();
        for sid in done {
            st.stopped.remove(&sid);
        }
    }
}
