//! The per-process and per-file readings a check takes, behind a trait
//! so a replay can answer them from a saved scene.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use crate::pidtree::{self, ProcId, ProcRow, ProcTable};

pub trait Probe {
    fn argv(&mut self, row: &ProcRow) -> Option<Arc<[String]>>;
    /// The file the process has as its standard output.
    fn stdout(&mut self, row: &ProcRow) -> Option<PathBuf>;
    fn cwd(&mut self, row: &ProcRow) -> Option<PathBuf>;
    fn exe(&mut self, row: &ProcRow) -> Option<PathBuf>;
    /// CPU of the process and the children it reaped, nanoseconds.
    fn cpu(&mut self, row: &ProcRow) -> Option<u64>;
    /// When `path` was last written, unix seconds; None when it is not there.
    fn written(&mut self, path: &Path) -> Option<u64>;
}

/// What a process does not change while it lives: read once per image.
#[derive(Default)]
struct Fixed {
    comm: String,
    argv: Option<Option<Arc<[String]>>>,
    stdout: Option<Option<PathBuf>>,
    cwd: Option<Option<PathBuf>>,
    exe: Option<Option<PathBuf>>,
}

/// The probe against the running system.  Readings that cannot change
/// for a process image are cached by `ProcId` and the command name, and
/// pruned to the processes of the table the check ran on.
#[derive(Default)]
pub struct Live {
    fixed: HashMap<ProcId, Fixed>,
    /// Syscalls made, for the cost tests.
    pub calls: u64,
}

impl Live {
    pub fn prune(&mut self, table: &ProcTable) {
        self.fixed.retain(|id, _| table.get(id.pid).is_some_and(|r| r.id() == *id));
    }

    fn entry(&mut self, row: &ProcRow) -> &mut Fixed {
        let f = self.fixed.entry(row.id()).or_default();
        // exec keeps the pid and start time but not the image
        if f.comm != row.comm {
            *f = Fixed { comm: row.comm.clone(), ..Fixed::default() };
        }
        f
    }

    /// `read` for `row`, kept only when the pid is still that process.
    fn checked<T>(&mut self, row: &ProcRow, read: impl FnOnce(i32) -> Option<T>) -> Option<T> {
        self.calls += 1;
        let v = read(row.pid)?;
        (pidtree::proc_id(row.pid) == Some(row.id())).then_some(v)
    }
}

impl Probe for Live {
    fn argv(&mut self, row: &ProcRow) -> Option<Arc<[String]>> {
        if let Some(v) = &self.entry(row).argv {
            return v.clone();
        }
        let v = self.checked(row, pidtree::proc_argv).map(Arc::from);
        self.entry(row).argv = Some(v.clone());
        v
    }

    fn stdout(&mut self, row: &ProcRow) -> Option<PathBuf> {
        if let Some(v) = &self.entry(row).stdout {
            return v.clone();
        }
        let v = self.checked(row, |pid| pidtree::fd_path(pid, 1));
        self.entry(row).stdout = Some(v.clone());
        v
    }

    fn cwd(&mut self, row: &ProcRow) -> Option<PathBuf> {
        if let Some(v) = &self.entry(row).cwd {
            return v.clone();
        }
        let v = self.checked(row, pidtree::proc_cwd);
        self.entry(row).cwd = Some(v.clone());
        v
    }

    fn exe(&mut self, row: &ProcRow) -> Option<PathBuf> {
        if let Some(v) = &self.entry(row).exe {
            return v.clone();
        }
        let v = self.checked(row, pidtree::exe_path);
        self.entry(row).exe = Some(v.clone());
        v
    }

    fn cpu(&mut self, row: &ProcRow) -> Option<u64> {
        self.checked(row, pidtree::cpu_with_children)
    }

    fn written(&mut self, path: &Path) -> Option<u64> {
        self.calls += 1;
        let m = std::fs::metadata(path).ok()?.modified().ok()?;
        Some(m.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()))
    }
}
