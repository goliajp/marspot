//! argv of the processes under the panes, cached per process image.

use std::collections::HashMap;
use std::sync::Arc;

use marspot::pidtree::{self, ProcId, ProcRow, ProcTable};

/// argv per process image.
///
/// Keyed by `ProcId` and checked against `comm` and the exec flag: exec
/// keeps the pid and the start time but replaces the image, and a child
/// read between its fork and its exec still carries its parent's argv.
/// `comm` alone misses an exec into a program of the same name (a shell
/// forking another shell), so that child would keep its parent's name
/// for good.  Pruned every build to the processes in the newest table,
/// so it is bounded by the live process set.
#[derive(Default)]
pub struct ArgvCache {
    map: HashMap<ProcId, (String, bool, Arc<[String]>)>,
    reads: u64,
}

impl ArgvCache {
    pub fn get(&mut self, row: &ProcRow) -> Option<Arc<[String]>> {
        if let Some((comm, execed, argv)) = self.map.get(&row.id())
            && *comm == row.comm
            && *execed == row.execed
        {
            return Some(argv.clone());
        }
        self.reads += 1;
        let argv: Arc<[String]> = pidtree::proc_argv(row.pid)?.into();
        // the pid may have been reused between the table and this read
        if pidtree::proc_id(row.pid) != Some(row.id()) {
            return None;
        }
        self.map.insert(row.id(), (row.comm.clone(), row.execed, argv.clone()));
        Some(argv)
    }

    pub fn prune(&mut self, table: &ProcTable) {
        self.map.retain(|id, _| table.get(id.pid).is_some_and(|r| r.id() == *id));
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[cfg(test)]
    pub fn reads(&self) -> u64 {
        self.reads
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};

    use super::*;

    fn until<T>(mut f: impl FnMut() -> Option<T>) -> Option<T> {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if let Some(v) = f() {
                return Some(v);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    /// A child held between its fork and its exec reads as its parent;
    /// once it execs a program of the same name it reads as itself.
    #[test]
    fn the_argv_cache_reads_a_child_again_once_it_has_execed() {
        let fifo = std::env::temp_dir().join(format!("mo-argv-exec-{}", std::process::id()));
        let _ = std::fs::remove_file(&fifo);
        assert!(std::process::Command::new("/usr/bin/mkfifo").arg(&fifo).status().unwrap().success());
        let script = format!("(read x < {}; exec /bin/bash -c 'sleep 30; :') & wait", fifo.display());
        let mut cmd = std::process::Command::new("/bin/bash");
        cmd.arg0("claude").args(["-c", &script]).process_group(0);
        let parent = marspot_term::test_env::ChildGuard::group(cmd.spawn().unwrap());
        let pid = parent.id() as i32;
        let held = until(|| ProcTable::take().children(pid).next().cloned());
        let held = held.expect("the forked child shows up");
        let mut cache = ArgvCache::default();
        let before = cache.get(&held).expect("argv of the held child");
        std::fs::write(&fifo, "go\n").unwrap();
        let execed = until(|| pidtree::proc_row(held.pid).filter(|r| r.execed && r.id() == held.id()));
        let after = execed.as_ref().and_then(|r| cache.get(r));
        drop(parent);
        let _ = std::fs::remove_file(&fifo);
        assert!(!held.execed, "a fork has not exec'd");
        assert_eq!(before.first().map(String::as_str), Some("claude"), "the fork carries its parent's argv");
        let execed = execed.expect("the child exec'd bash");
        assert_eq!(execed.comm, held.comm, "the same program name before and after the exec");
        assert_eq!(after.as_deref().and_then(|a| a.first()).map(String::as_str), Some("/bin/bash"));
    }
}
