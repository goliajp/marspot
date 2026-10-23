//! marspot's pane names, as every messaging party reads them.
//!
//! A session in a marspot pane can be addressed by the pane's name
//! (`gds`, `spg#2`) whatever the session calls itself.  marspot keeps
//! that table and nobody else writes it: one file per marspot, in each
//! registry directory, beside the records -- the one place the Codex
//! MCP server, the Claude Code hook and the command line all look,
//! whatever state directory marspot itself was started with.  The dot
//! keeps Claude Code, which reads `<pid>.json`, from taking it for a
//! session.
//!
//! The file names the marspot that wrote it.  A reader drops a table
//! whose writer is gone, and an entry whose session is gone, so a quit
//! or crashed marspot leaves no name pointing anywhere.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::json::{self, Value, obj};
use crate::proc;
use crate::registry::{self, Entry};

const PREFIX: &str = ".marspot-panes-";

/// One pane that runs exactly one agent session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pane {
    /// The pane's name, as marspot draws it.
    pub name: String,
    /// The pid of the session's registry record.
    pub pid: i32,
    /// The session's inbox socket.
    pub socket: String,
}

/// The table one marspot writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    /// The writing marspot: its state root, pid and start time.
    pub owner: String,
    pub pid: i32,
    pub proc_start: String,
    pub panes: Vec<Pane>,
}

impl Table {
    fn to_json(&self) -> String {
        let panes = self
            .panes
            .iter()
            .map(|p| {
                obj([
                    ("name", Value::from(p.name.as_str())),
                    ("pid", Value::from(i64::from(p.pid))),
                    ("socket", Value::from(p.socket.as_str())),
                ])
            })
            .collect();
        obj([
            ("owner", Value::from(self.owner.as_str())),
            ("pid", Value::from(i64::from(self.pid))),
            ("procStart", Value::from(self.proc_start.as_str())),
            ("panes", Value::Arr(panes)),
        ])
        .to_json()
    }

    fn parse(text: &str) -> Option<Table> {
        let v = json::parse(text).ok()?;
        let panes = v
            .get("panes")
            .map_or(&[][..], Value::arr)
            .iter()
            .filter_map(|p| {
                Some(Pane {
                    name: p.str_at("name")?.to_string(),
                    pid: i32::try_from(p.get("pid")?.i64()?).ok()?,
                    socket: p.str_at("socket")?.to_string(),
                })
            })
            .collect();
        Some(Table {
            owner: v.str_at("owner")?.to_string(),
            pid: i32::try_from(v.get("pid")?.i64()?).ok()?,
            proc_start: v.str_at("procStart")?.to_string(),
            panes,
        })
    }

    fn writer_alive(&self) -> bool {
        proc::lstart(self.pid).as_deref() == Some(self.proc_start.as_str())
    }
}

/// The file `owner` writes in `dir`.
pub fn path_for(dir: &Path, owner: &str) -> PathBuf {
    // FNV-1a: a short, stable name for a long path
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in owner.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    dir.join(format!("{PREFIX}{h:016x}.json"))
}

/// Write `table` atomically into every registry directory.
pub fn write(table: &Table) -> io::Result<()> {
    for dir in registry::default_dirs() {
        let path = path_for(&dir, &table.owner);
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, table.to_json())?;
        fs::rename(&tmp, &path)?;
    }
    Ok(())
}

/// Remove `owner`'s table.
pub fn remove(owner: &str) {
    for dir in registry::default_dirs() {
        let _ = fs::remove_file(path_for(&dir, owner));
    }
}

/// Every pane of every live marspot, each still naming a live session
/// in `entries`.
pub fn live(entries: &[Entry]) -> Vec<Pane> {
    let mut out: Vec<Pane> = Vec::new();
    for dir in registry::default_dirs() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for f in rd.flatten() {
            let name = f.file_name();
            let Some(name) = name.to_str() else { continue };
            if !(name.starts_with(PREFIX) && name.ends_with(".json")) {
                continue;
            }
            let Some(t) = fs::read_to_string(f.path()).ok().and_then(|s| Table::parse(&s)) else { continue };
            if !t.writer_alive() {
                continue;
            }
            for p in t.panes {
                let live = entries.iter().any(|e| e.pid == p.pid && e.socket.as_deref() == Some(p.socket.as_str()));
                if live && !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// The pane `name` means, when exactly one live pane has it.  Two live
/// marspots can each have a `spg`; then the name means neither.
pub fn find<'a>(panes: &'a [Pane], name: &str) -> Result<Option<&'a Pane>, String> {
    let hits: Vec<&Pane> = panes.iter().filter(|p| p.name == name).collect();
    match hits.as_slice() {
        [] => Ok(None),
        [p] => Ok(Some(p)),
        many => Err(format!(
            "{} panes are named {name:?}; use one of their addresses instead: {}",
            many.len(),
            many.iter().map(|p| format!("uds:{}", p.socket)).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// The pane a session sits in, by its record's pid.
pub fn of_pid(panes: &[Pane], pid: i32) -> Option<&Pane> {
    panes.iter().find(|p| p.pid == pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_reads_back_and_names_its_writer() {
        let me = std::process::id() as i32;
        let t = Table {
            owner: "/tmp/owner".into(),
            pid: me,
            proc_start: proc::lstart(me).unwrap(),
            panes: vec![Pane { name: "spg#2".into(), pid: 7, socket: "/tmp/cc-socks/7.sock".into() }],
        };
        let back = Table::parse(&t.to_json()).unwrap();
        assert_eq!(back, t);
        assert!(back.writer_alive());
        let gone = Table { proc_start: "Thu Jan  1 00:00:00 1970".into(), ..t };
        assert!(!gone.writer_alive(), "a recycled pid is not the writer");
    }

    #[test]
    fn a_shared_name_means_no_pane() {
        let p = |n: &str, pid| Pane { name: n.into(), pid, socket: format!("/tmp/cc-socks/{pid}.sock") };
        let panes = vec![p("gds", 1), p("spg", 2), p("spg", 3)];
        assert_eq!(find(&panes, "gds").unwrap().map(|p| p.pid), Some(1));
        assert_eq!(find(&panes, "nope").unwrap(), None);
        assert!(find(&panes, "spg").unwrap_err().contains("uds:/tmp/cc-socks/2.sock"));
        assert_eq!(of_pid(&panes, 3).map(|p| p.name.as_str()), Some("spg"));
    }

    #[test]
    fn file_names_are_per_owner_and_hidden() {
        let a = path_for(Path::new("/r"), "/x/a");
        assert_ne!(a, path_for(Path::new("/r"), "/x/b"));
        let n = a.file_name().unwrap().to_str().unwrap();
        assert!(n.starts_with('.') && n.parse::<i32>().is_err());
    }
}
