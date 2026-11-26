//! A service's directory under the state root: `<state>/<name>/`, by the
//! names the directories have today (`helpers`, `plugins/claudecode`,
//! `zombie` ...), so moving a writer here moves no file.

use std::io;
use std::path::{Path, PathBuf};

use crate::host::state_map;

use super::fact_log::FactLog;

pub(crate) struct StateStore {
    name: String,
    dir: PathBuf,
}

impl StateStore {
    /// `<state root>/<name>/`, made when it is not there.
    pub(crate) fn open(name: &str) -> io::Result<StateStore> {
        StateStore::open_in(&marspot_term::paths::state_root(), name)
    }

    /// The same under another root.
    pub(crate) fn open_in(root: &Path, name: &str) -> io::Result<StateStore> {
        super::assert_off_main("a state directory opened");
        debug_assert!(state_map::has_dir(name), "{name}/ is not in host::state_map");
        let dir = root.join(name);
        std::fs::create_dir_all(&dir)?;
        Ok(StateStore { name: name.to_string(), dir })
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// The path of the file `rel` in this directory, which the state map
    /// must know.
    pub(crate) fn file(&self, rel: &str) -> PathBuf {
        debug_assert!(
            state_map::lookup(&format!("{}/{rel}", self.name)).is_some_and(|e| !e.path.starts_with("**")),
            "{}/{rel} is not in host::state_map",
            self.name
        );
        self.dir.join(rel)
    }

    /// The fact log in the subdirectory `sub`: one file a day, at most
    /// `cap` bytes of facts a day, `keep_days` days kept.
    pub(crate) fn facts(&self, sub: &str, cap: u64, keep_days: u32) -> io::Result<FactLog> {
        debug_assert!(state_map::has_dir(&format!("{}/{sub}", self.name)), "{}/{sub}/ is not in host::state_map", self.name);
        FactLog::at(self.dir.join(sub), cap, keep_days)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_is_the_directory_of_today_and_names_only_registered_files() {
        let root = marspot_term::test_env::TempDir::new("state-store");
        let store = StateStore::open_in(root.path(), "zombie").unwrap();
        assert_eq!(store.dir(), root.join("zombie"));
        assert!(store.dir().is_dir());
        assert_eq!(store.file("told.tsv"), root.join("zombie/told.tsv"));
        let claude = StateStore::open_in(root.path(), "plugins/claudecode").unwrap();
        assert_eq!(claude.file("model/2fec8734-bf76-478b-8044-20261126b7e4").parent().unwrap(), root.join("plugins/claudecode/model"));
    }

    #[test]
    fn a_store_opens_under_the_state_root_and_keeps_its_facts_by_day() {
        let sandbox = marspot_term::test_env::sandbox_state_dir();
        let store = StateStore::open("plugins/claudecode").unwrap();
        assert_eq!(store.dir(), sandbox.path().join("plugins/claudecode"));
        let mut facts = store.facts("alloc_events", 1 << 20, 15).unwrap();
        assert!(facts.append(1_764_115_200, "moved", &[("to", "2")]).unwrap());
        let day = std::fs::read_to_string(sandbox.path().join("plugins/claudecode/alloc_events/20251126.tsv")).unwrap();
        assert_eq!(day, "1764115200\tmoved\tto=2\n");
    }

    #[test]
    #[should_panic(expected = "not in host::state_map")]
    fn an_unregistered_file_is_refused_in_a_test_build() {
        let root = marspot_term::test_env::TempDir::new("state-store-unregistered");
        let _ = StateStore::open_in(root.path(), "zombie").unwrap().file("surprise.tsv");
    }

    #[test]
    #[should_panic(expected = "not in host::state_map")]
    fn an_unregistered_directory_is_refused_in_a_test_build() {
        let root = marspot_term::test_env::TempDir::new("state-store-undir");
        let _ = StateStore::open_in(root.path(), "nobody-registered-this");
    }
}
