//! marspot's pane names, as every messaging party reads them.
//!
//! A session in a marspot pane can be addressed by the pane's name
//! (`gds`, `spg#3381`) whatever the session calls itself.  marspot keeps
//! that table and nobody else writes it: one file per marspot, in each
//! registry directory, beside the records -- the one place the Codex
//! MCP server, the Claude Code hook and the command line all look,
//! whatever state directory marspot itself was started with.  The dot
//! keeps Claude Code, which reads `<pid>.json`, from taking it for a
//! session.
//!
//! An entry's `code`, `base` and `agent` are optional: a reader that
//! does not know them matches on `name`, and an entry without them is
//! matched on `name` alone.
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
use crate::resolve::Target;

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
    /// The pane's identity code and its directory's name; `base#code`
    /// is its full name.
    pub code: Option<u16>,
    pub base: Option<String>,
    /// The agent in it: `claude`, `codex`, `gemini` or `kimi`.
    pub agent: Option<String>,
}

impl Pane {
    /// The pane as name resolution sees it.
    pub fn target(&self) -> Target<'_> {
        match (&self.base, self.code) {
            (Some(base), Some(_)) => Target { base, code: self.code, kind: self.agent.as_deref() },
            _ => Target { base: &self.name, code: None, kind: self.agent.as_deref() },
        }
    }

    /// `base#code`, when the name drawn on the pane is the base alone.
    pub fn full_name_if_not_shown(&self) -> Option<String> {
        let full = self.target().full_name();
        (full != self.name).then_some(full)
    }
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
                let mut v = obj([
                    ("name", Value::from(p.name.as_str())),
                    ("pid", Value::from(i64::from(p.pid))),
                    ("socket", Value::from(p.socket.as_str())),
                ]);
                if let Some(code) = p.code {
                    v.set("code", Value::from(i64::from(code)));
                }
                if let Some(base) = &p.base {
                    v.set("base", Value::from(base.as_str()));
                }
                if let Some(agent) = &p.agent {
                    v.set("agent", Value::from(agent.as_str()));
                }
                v
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
                    code: p.get("code").and_then(Value::i64).and_then(|c| u16::try_from(c).ok()),
                    base: p.str_at("base").map(str::to_string),
                    agent: p.str_at("agent").map(str::to_string),
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

/// The pane a session sits in, by its record's pid.
pub fn of_pid(panes: &[Pane], pid: i32) -> Option<&Pane> {
    panes.iter().find(|p| p.pid == pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(name: &str, pid: i32) -> Pane {
        Pane { name: name.into(), pid, socket: format!("/tmp/cc-socks/{pid}.sock"), code: None, base: None, agent: None }
    }

    #[test]
    fn a_table_reads_back_and_names_its_writer() {
        let me = std::process::id() as i32;
        let coded = Pane { code: Some(3381), base: Some("spg".into()), agent: Some("codex".into()), ..pane("spg#3381", 7) };
        let t = Table { owner: "/tmp/owner".into(), pid: me, proc_start: proc::lstart(me).unwrap(), panes: vec![coded, pane("gds", 8)] };
        let back = Table::parse(&t.to_json()).unwrap();
        assert_eq!(back, t);
        assert!(back.writer_alive());
        assert_eq!(back.panes[0].target(), Target { base: "spg", code: Some(3381), kind: Some("codex") });
        assert_eq!(back.panes[0].full_name_if_not_shown(), None);
        let lone = Pane { code: Some(3381), base: Some("spg".into()), ..pane("spg", 7) };
        assert_eq!(lone.full_name_if_not_shown().as_deref(), Some("spg#3381"));
        let gone = Table { proc_start: "Thu Jan  1 00:00:00 1970".into(), ..t };
        assert!(!gone.writer_alive(), "a recycled pid is not the writer");
    }

    /// A table written before entries had a code is read, each entry
    /// matched on the name it was listed under.
    #[test]
    fn a_table_without_the_newer_fields_is_read() {
        let old = r#"{"owner":"/o","pid":1,"procStart":"x","panes":[{"name":"spg#2","pid":7,"socket":"/tmp/cc-socks/7.sock"}]}"#;
        let t = Table::parse(old).unwrap();
        assert_eq!(t.panes, vec![pane("spg#2", 7)]);
        assert_eq!(t.panes[0].target(), Target { base: "spg#2", code: None, kind: None });
        assert_eq!(of_pid(&t.panes, 7).map(|p| p.name.as_str()), Some("spg#2"));
    }

    /// The reader of the version before codes took `name`, `pid` and
    /// `socket` from each entry and nothing else; it accepts a table
    /// with the newer fields and finds the pane by the name drawn.
    #[test]
    fn a_reader_that_knows_three_fields_reads_the_newer_table() {
        let t = Table {
            owner: "/o".into(),
            pid: 1,
            proc_start: "x".into(),
            panes: vec![Pane { code: Some(3381), base: Some("spg".into()), agent: Some("claude".into()), ..pane("spg#3381", 7) }],
        };
        let v = json::parse(&t.to_json()).unwrap();
        let old_reader: Vec<(String, i32, String)> = v
            .get("panes")
            .map_or(&[][..], Value::arr)
            .iter()
            .filter_map(|p| Some((p.str_at("name")?.to_string(), i32::try_from(p.get("pid")?.i64()?).ok()?, p.str_at("socket")?.to_string())))
            .collect();
        assert_eq!(old_reader, vec![("spg#3381".to_string(), 7, "/tmp/cc-socks/7.sock".to_string())]);
        assert!(v.str_at("owner").is_some() && v.get("pid").is_some() && v.str_at("procStart").is_some());
    }

    #[test]
    fn file_names_are_per_owner_and_hidden() {
        let a = path_for(Path::new("/r"), "/x/a");
        assert_ne!(a, path_for(Path::new("/r"), "/x/b"));
        let n = a.file_name().unwrap().to_str().unwrap();
        assert!(n.starts_with('.') && n.parse::<i32>().is_err());
    }
}
