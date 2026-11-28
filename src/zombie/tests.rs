//! What the watch's tests share: a probe answered from maps, and rows.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::pidtree::ProcRow;

use super::probe::Probe;

mod history;
mod replay;

/// Readings from maps, keyed by pid and path.
#[derive(Default)]
pub(super) struct MapProbe {
    pub argv: HashMap<i32, Vec<String>>,
    pub stdout: HashMap<i32, PathBuf>,
    pub cwd: HashMap<i32, PathBuf>,
    pub exe: HashMap<i32, PathBuf>,
    pub cpu: HashMap<i32, u64>,
    pub written: HashMap<PathBuf, u64>,
}

impl Probe for MapProbe {
    fn argv(&mut self, row: &ProcRow) -> Option<Arc<[String]>> {
        self.argv.get(&row.pid).map(|a| Arc::from(a.clone()))
    }
    fn stdout(&mut self, row: &ProcRow) -> Option<PathBuf> {
        self.stdout.get(&row.pid).cloned()
    }
    fn cwd(&mut self, row: &ProcRow) -> Option<PathBuf> {
        self.cwd.get(&row.pid).cloned()
    }
    fn exe(&mut self, row: &ProcRow) -> Option<PathBuf> {
        self.exe.get(&row.pid).cloned()
    }
    fn cpu(&mut self, row: &ProcRow) -> Option<u64> {
        self.cpu.get(&row.pid).copied()
    }
    fn written(&mut self, path: &Path) -> Option<u64> {
        self.written.get(path).copied()
    }
}

pub(super) fn row(pid: i32, ppid: i32, pgid: i32, start: u64, comm: &str) -> ProcRow {
    ProcRow {
        pid,
        ppid,
        comm: comm.to_string(),
        start_unix: start,
        start_us: start * 1_000_000,
        pgid,
        tty_dev: 0,
        tty_fg_pgid: 0,
        status: libc::SRUN,
        execed: true,
    }
}

/// claude's resting helpers by name, as the shell's list has them.
pub(super) fn helper(r: &ProcRow, has_children: bool) -> bool {
    r.comm.contains("-mcp") || r.comm.starts_with("rust-analyzer") || r.comm == "caffeinate" || (r.comm == "zsh" && !has_children)
}

pub(super) fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/zombie")
}
