//! The registry as a reader sees it: parsed once per file version, and
//! one rule for whether an entry's L3 is alive.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::{SessionEntry, parse_session_entry_text, session_entry_path};
use crate::paths::sessions_dir;

/// A process started this long after its entry was written still counts
/// as the writer: covers clock steps and the Linux start time's
/// one-second resolution.
const START_SLACK_US: u64 = 2_000_000;

/// Which version of an `entry.toml` was parsed.  The writer renames a
/// new file into place, so a rewrite always changes the inode even when
/// size and mtime match.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FileKey {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_ns: i128,
}

impl FileKey {
    fn of(meta: &std::fs::Metadata) -> FileKey {
        FileKey {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime_ns: meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128,
        }
    }

    fn mtime_us(&self) -> u64 {
        u64::try_from(self.mtime_ns / 1_000).unwrap_or(0)
    }
}

enum Slot {
    Parsed { key: FileKey, entry: SessionEntry },
    Bad { key: Option<FileKey>, why: String },
}

/// `list_session_entries` that re-reads only what changed.
///
/// Each call lists `sessions/` and stats every `<id>/entry.toml`; a file
/// is read and parsed again only when its (dev, inode, size, mtime)
/// changed.  A dir without `entry.toml` is not reported: an L3 creates
/// its dir before it writes the entry.
#[derive(Default)]
pub struct RegistryCache {
    slots: BTreeMap<u64, Slot>,
    entries: Vec<SessionEntry>,
    problems: Vec<(u64, String)>,
    parses: u64,
}

impl RegistryCache {
    /// Every entry that parses, in id order.
    pub fn list(&mut self) -> &[SessionEntry] {
        self.refresh(&sessions_dir());
        &self.entries
    }

    /// Session dirs whose `entry.toml` could not be read or parsed, as
    /// of the last `list`.
    pub fn problems(&self) -> &[(u64, String)] {
        &self.problems
    }

    /// The mtime of the entry file last parsed for `id`, microseconds.
    pub fn written_us(&self, id: u64) -> Option<u64> {
        match self.slots.get(&id)? {
            Slot::Parsed { key, .. } => Some(key.mtime_us()),
            Slot::Bad { .. } => None,
        }
    }

    fn refresh(&mut self, root: &Path) {
        let mut changed = false;
        let mut seen: Vec<u64> = Vec::with_capacity(self.slots.len());
        match std::fs::read_dir(root) {
            Ok(dir) => {
                for de in dir.flatten() {
                    if !de.file_type().is_ok_and(|t| t.is_dir()) {
                        continue;
                    }
                    let Some(id) = de.file_name().to_str().and_then(|n| n.parse::<u64>().ok())
                    else {
                        continue;
                    };
                    if self.refresh_one(id) {
                        changed = true;
                    }
                    if self.slots.contains_key(&id) {
                        seen.push(id);
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                crate::lx_warn!("session_registry.list_failed", &format!("{e}"));
            }
        }
        seen.sort_unstable();
        let before = self.slots.len();
        self.slots.retain(|id, _| seen.binary_search(id).is_ok());
        changed |= self.slots.len() != before;
        if changed {
            self.rebuild();
        }
    }

    /// Bring one slot up to date.  True when it changed.
    fn refresh_one(&mut self, id: u64) -> bool {
        let path = session_entry_path(id);
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return self.slots.remove(&id).is_some();
            }
            Err(e) => return self.set_bad(id, None, format!("stat: {e}")),
        };
        let key = FileKey::of(&meta);
        let current = match self.slots.get(&id) {
            Some(Slot::Parsed { key: k, .. }) => Some(*k),
            Some(Slot::Bad { key: k, .. }) => *k,
            None => None,
        };
        if current == Some(key) {
            return false;
        }
        self.parses += 1;
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| format!("read: {e}"))
            .and_then(|text| parse_session_entry_text(&text).map_err(|e| format!("parse: {e}")));
        match parsed {
            Ok(entry) => {
                self.slots.insert(id, Slot::Parsed { key, entry });
                true
            }
            Err(why) => self.set_bad(id, Some(key), why),
        }
    }

    /// Record a bad entry, warning only when this is news.
    fn set_bad(&mut self, id: u64, key: Option<FileKey>, why: String) -> bool {
        if let Some(Slot::Bad { key: k, why: w }) = self.slots.get(&id)
            && *k == key
            && *w == why
        {
            return false;
        }
        crate::lx_warn!("session_registry.entry_unusable", &why, session_id = id);
        self.slots.insert(id, Slot::Bad { key, why });
        true
    }

    fn rebuild(&mut self) {
        self.entries.clear();
        self.problems.clear();
        for (&id, slot) in &self.slots {
            match slot {
                Slot::Parsed { entry, .. } => self.entries.push(entry.clone()),
                Slot::Bad { why, .. } => self.problems.push((id, why.clone())),
            }
        }
    }
}

/// Whether the L3 that wrote `entry` is still running.
///
/// This is the one rule; every other liveness test is a weaker copy of
/// it.  A pid that exists is not enough (pids are recycled, and an
/// unrelated process may hold it), nor is `kill(pid, 0)` failing with
/// EPERM a death.  The entry's process is alive when the pid runs an
/// executable named `marspot-session` and that process started no later
/// than the entry file was last written, plus a small slack: a recycled
/// pid belongs to a process that started after the file was written.
pub fn entry_is_live(entry: &SessionEntry) -> bool {
    let Ok(meta) = std::fs::metadata(session_entry_path(entry.id)) else {
        return false;
    };
    let Some(start_us) = crate::platform::pid_start_us(entry.pid) else {
        return false;
    };
    entry_is_live_with(entry, FileKey::of(&meta).mtime_us(), start_us)
}

/// `entry_is_live` with the entry file's mtime and the process's start
/// time already known (from `RegistryCache::written_us` and a process
/// table), so only the executable is looked up.
pub fn entry_is_live_with(entry: &SessionEntry, written_us: u64, proc_start_us: u64) -> bool {
    entry.pid > 1
        && proc_start_us <= written_us.saturating_add(START_SLACK_US)
        && crate::platform::pid_exe_name(entry.pid).is_some_and(|n| n == "marspot-session")
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{StateDirGuard, sample_entry};
    use super::super::{session_dir, write_session_entry};
    use super::*;
    use std::time::{Duration, Instant};

    fn set_mtime(path: &Path, sec: i64, nsec: i64) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let t = libc::timespec {
            tv_sec: sec as _,
            tv_nsec: nsec as _,
        };
        let r = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [t, t].as_ptr(), 0) };
        assert_eq!(r, 0, "utimensat: {}", io::Error::last_os_error());
    }

    fn ids(c: &mut RegistryCache) -> Vec<u64> {
        c.list().iter().map(|e| e.id).collect()
    }

    #[test]
    fn unchanged_files_are_not_parsed_again_and_removed_ids_drop_out() {
        let _g = StateDirGuard::new();
        write_session_entry(&sample_entry(1)).unwrap();
        write_session_entry(&sample_entry(2)).unwrap();
        let mut c = RegistryCache::default();
        assert_eq!(ids(&mut c), vec![1, 2]);
        assert_eq!(c.parses, 2);
        assert_eq!(ids(&mut c), vec![1, 2]);
        assert_eq!(c.parses, 2, "nothing changed, nothing re-parsed");
        std::fs::remove_dir_all(session_dir(1)).unwrap();
        assert_eq!(ids(&mut c), vec![2]);
        assert!(c.written_us(1).is_none());
        assert!(c.written_us(2).is_some());
    }

    /// Same size, same second: only the inode tells the versions apart.
    #[test]
    fn a_same_size_rewrite_within_the_second_is_seen() {
        let _g = StateDirGuard::new();
        let mut e = sample_entry(3);
        e.title = "aaa".into();
        write_session_entry(&e).unwrap();
        let mut c = RegistryCache::default();
        assert_eq!(c.list()[0].title, "aaa");
        let before = std::fs::metadata(session_entry_path(3)).unwrap();
        e.title = "bbb".into();
        write_session_entry(&e).unwrap();
        set_mtime(&session_entry_path(3), before.mtime(), before.mtime_nsec());
        let after = std::fs::metadata(session_entry_path(3)).unwrap();
        assert_eq!(before.size(), after.size());
        assert_eq!(
            (before.mtime(), before.mtime_nsec()),
            (after.mtime(), after.mtime_nsec())
        );
        assert_eq!(c.list()[0].title, "bbb");
        assert_eq!(c.parses, 2);
    }

    #[test]
    fn a_cwd_with_newlines_round_trips() {
        let _g = StateDirGuard::new();
        let mut e = sample_entry(4);
        e.cwd = "/tmp/a\nb\r\nc \\n \"q\"\\".into();
        e.title = "t\nitle".into();
        write_session_entry(&e).unwrap();
        let text = std::fs::read_to_string(session_entry_path(4)).unwrap();
        assert_eq!(text.lines().count(), 12, "a value split a line:\n{text}");
        let mut c = RegistryCache::default();
        assert_eq!(c.list(), &[e]);
    }

    /// A file as the writer before the newline escapes produced it.
    #[test]
    fn an_old_format_file_reads_the_same() {
        let _g = StateDirGuard::new();
        let old = "# marspot session registry entry — RFC-003\nid = 5\npid = 123\n\
            socket = \"/s/5/sock\"\ncols = 80\nrows = 24\ntitle = \"a \\\"b\\\" c\\\\n\"\n\
            cwd = \"C:\\\\dir\\\\new\"\nproto_version = 3\ncreated_at_unix = 1700000000\n\
            shm_name = \"/msp-s-5\"\nshell_child_pid = 456\n";
        std::fs::create_dir_all(session_dir(5)).unwrap();
        std::fs::write(session_entry_path(5), old).unwrap();
        let mut c = RegistryCache::default();
        let got = c.list()[0].clone();
        assert_eq!(got.title, "a \"b\" c\\n");
        assert_eq!(got.cwd, "C:\\dir\\new");
        assert_eq!(got.shell_child_pid, 456);
    }

    #[test]
    fn empty_and_truncated_entries_are_reported_not_hidden() {
        let _g = StateDirGuard::new();
        write_session_entry(&sample_entry(6)).unwrap();
        let full = std::fs::read_to_string(session_entry_path(6)).unwrap();
        std::fs::create_dir_all(session_dir(7)).unwrap();
        std::fs::write(session_entry_path(7), "").unwrap();
        std::fs::create_dir_all(session_dir(8)).unwrap();
        std::fs::write(session_entry_path(8), &full[..full.len() / 2]).unwrap();
        std::fs::create_dir_all(session_dir(9)).unwrap();
        let mut c = RegistryCache::default();
        assert_eq!(ids(&mut c), vec![6]);
        let bad: Vec<u64> = c.problems().iter().map(|(id, _)| *id).collect();
        assert_eq!(bad, vec![7, 8], "a dir with no entry yet is not a problem");
        assert_eq!(ids(&mut c), vec![6]);
        assert_eq!(c.parses, 3, "a bad file is not re-read until it changes");
        assert_eq!(c.problems().len(), 2);
        write_session_entry(&sample_entry(7)).unwrap();
        assert_eq!(ids(&mut c), vec![6, 7]);
        assert_eq!(c.problems().len(), 1);
    }

    #[test]
    fn an_entry_naming_a_foreign_or_missing_pid_is_not_live() {
        let _g = StateDirGuard::new();
        let mut e = sample_entry(10);
        e.pid = std::process::id() as i32;
        write_session_entry(&e).unwrap();
        assert!(!entry_is_live(&e), "the test binary is not marspot-session");
        e.pid = i32::MAX;
        assert!(!entry_is_live(&e), "no such pid");
        e.pid = 1;
        assert!(!entry_is_live_with(&e, u64::MAX, 0));
    }

    /// A process whose executable is named `marspot-session` is live
    /// only if it started before its entry was written.
    #[test]
    fn a_session_executable_is_live_until_its_pid_is_younger_than_the_entry() {
        let _g = StateDirGuard::new();
        let bin = session_dir(11).join("marspot-session");
        std::fs::create_dir_all(session_dir(11)).unwrap();
        std::fs::copy("/bin/sleep", &bin).unwrap();
        let mut child = std::process::Command::new(&bin).arg("30").spawn().unwrap();
        let pid = child.id() as i32;
        // the child's image is ours until it has exec'd
        let deadline = Instant::now() + Duration::from_secs(5);
        while crate::platform::pid_exe_name(pid).as_deref() != Some("marspot-session")
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut e = sample_entry(11);
        e.pid = pid;
        write_session_entry(&e).unwrap();
        let live = entry_is_live(&e);
        let start = crate::platform::pid_start_us(pid).unwrap();
        let reused = entry_is_live_with(&e, start - START_SLACK_US - 1, start);
        let _ = child.kill();
        let _ = child.wait();
        assert!(live, "a marspot-session started before its entry is live");
        assert!(!reused, "a process younger than its entry is a reused pid");
        assert!(!entry_is_live(&e), "a reaped process is not live");
    }
}
