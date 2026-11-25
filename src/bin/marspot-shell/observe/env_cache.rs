//! Environment values of agent processes, cached per process image.

use std::collections::HashMap;

use marspot::pidtree::{self, ProcId, ProcRow};

use super::Observation;

/// Environment values of agent processes, one read per process image
/// and key.
///
/// Owned by each consumer rather than shared: the claudecode worker
/// asks about `claude` processes and codex about `codex` ones, each on
/// its own thread, so a shared map would guard nothing but add a lock
/// to the main thread.  Absent values are kept too: a default profile
/// has no `CLAUDE_CONFIG_DIR`, and that answer does not change either.
#[derive(Default)]
pub struct EnvCache {
    map: HashMap<(ProcId, &'static str), (String, Option<String>)>,
    reads: u64,
}

impl EnvCache {
    pub fn value(&mut self, row: &ProcRow, key: &'static str) -> Option<String> {
        if let Some((comm, v)) = self.map.get(&(row.id(), key))
            && *comm == row.comm
        {
            return v.clone();
        }
        self.reads += 1;
        let v = pidtree::proc_env_value(row.pid, key);
        if pidtree::proc_id(row.pid) != Some(row.id()) {
            return None;
        }
        self.map.insert((row.id(), key), (row.comm.clone(), v.clone()));
        v
    }

    /// Whether every one of `keys` is already known for `row`.
    pub fn has_all(&self, row: &ProcRow, keys: &[&'static str]) -> bool {
        keys.iter().all(|k| self.map.get(&(row.id(), *k)).is_some_and(|(comm, _)| *comm == row.comm))
    }

    /// Read `keys` of `row` together, one read for the whole
    /// environment, so later `value` calls for them are answered from
    /// the cache.  False when the process is gone or not the one `row`
    /// names.
    pub fn load(&mut self, row: &ProcRow, keys: &[&'static str]) -> bool {
        self.reads += 1;
        let values = pidtree::proc_env_values(row.pid, keys);
        if pidtree::proc_id(row.pid) != Some(row.id()) {
            return false;
        }
        let values = values.unwrap_or_else(|| vec![None; keys.len()]);
        for (k, v) in keys.iter().zip(values) {
            self.map.insert((row.id(), *k), (row.comm.clone(), v));
        }
        true
    }

    pub fn prune(&mut self, obs: &Observation) {
        self.map.retain(|(id, _), _| obs.has(*id));
    }

    /// Environment reads made so far, cached answers not counted.
    #[cfg(test)]
    pub fn reads(&self) -> u64 {
        self.reads
    }
}
