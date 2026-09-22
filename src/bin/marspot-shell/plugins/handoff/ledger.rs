//! Per-pane memory of which session each agent has in this pane, and
//! how much of it has been handed over.
//!
//! This is what makes switching back and forth cheap and clean: the
//! second time a pane goes to an agent it RESUMES the session it had
//! there, instead of starting another one, and only the part the other
//! side has not seen is handed over.  Two small files per pane, the
//! ledger and the last document, both overwritten on every switch and
//! both removed once the pane is gone.

use std::path::{Path, PathBuf};

use super::transcript::Agent;

/// One agent's session in this pane.
#[derive(Debug, Clone, PartialEq)]
pub struct Side {
    /// claude's session uuid, codex's thread id.
    pub id: String,
    /// How far into that session's history has been handed over.
    pub offset: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ledger {
    pub claude: Option<Side>,
    pub codex: Option<Side>,
}

impl Ledger {
    pub fn side(&self, a: Agent) -> Option<&Side> {
        match a {
            Agent::Claude => self.claude.as_ref(),
            Agent::Codex => self.codex.as_ref(),
        }
    }
    pub fn set(&mut self, a: Agent, s: Option<Side>) {
        match a {
            Agent::Claude => self.claude = s,
            Agent::Codex => self.codex = s,
        }
    }

    /// Where to start reading `a`'s session `id`: the bookmark if it is
    /// the same session, the beginning if the pane moved on to another
    /// one (a `/clear`, a session the user started by hand).
    pub fn watermark(&self, a: Agent, id: &str) -> u64 {
        self.side(a).filter(|s| s.id == id).map_or(0, |s| s.offset)
    }

    fn encode(&self) -> String {
        let mut out = String::new();
        for (a, s) in [(Agent::Claude, &self.claude), (Agent::Codex, &self.codex)] {
            if let Some(s) = s {
                out.push_str(&format!("{} {} {}\n", a.key(), s.id, s.offset));
            }
        }
        out
    }

    fn decode(text: &str) -> Self {
        let mut l = Self::default();
        for line in text.lines() {
            let mut it = line.split(' ');
            let (Some(k), Some(id), Some(off)) = (it.next(), it.next(), it.next()) else { continue };
            let Ok(offset) = off.parse() else { continue };
            let side = Some(Side { id: id.to_string(), offset });
            match k {
                "claude" => l.claude = side,
                "codex" => l.codex = side,
                _ => {}
            }
        }
        l
    }

    /// The pane's ledger; empty when it has none (never switched).
    pub fn load(dir: &Path, sid: u64) -> Self {
        std::fs::read_to_string(ledger_path(dir, sid)).map(|t| Self::decode(&t)).unwrap_or_default()
    }

    pub fn save(&self, dir: &Path, sid: u64) -> std::io::Result<()> {
        write_atomic(&ledger_path(dir, sid), self.encode().as_bytes())
    }
}

/// `<state>/handoff` — shared by both agent plugins, so not either
/// one's plugin state dir.
pub fn dir() -> PathBuf {
    marspot::paths::state_root().join("handoff")
}

fn ledger_path(dir: &Path, sid: u64) -> PathBuf {
    dir.join(format!("{sid}.ledger"))
}

/// The one handoff document a pane has.
pub fn doc_path(dir: &Path, sid: u64) -> PathBuf {
    dir.join(format!("{sid}.md"))
}

/// Replace a file in one step, so a reader never sees half of it —
/// the receiving agent may open the document the instant it is named.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Remove every pane's files except the live ones'.
///
/// Run on each switch rather than on a timer: nothing here grows while
/// nobody switches, and a pane that closed is gone by the next switch.
pub fn prune(dir: &Path, live: &[u64]) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let mut removed = 0;
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some((stem, ext)) = name.split_once('.') else { continue };
        if !matches!(ext, "ledger" | "md" | "tmp") {
            continue;
        }
        let Ok(sid) = stem.parse::<u64>() else { continue };
        if !live.contains(&sid) && std::fs::remove_file(e.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::handoff::history::tmp;

    #[test]
    fn a_ledger_round_trips() {
        let d = tmp("ledger");
        let mut l = Ledger::default();
        l.set(Agent::Claude, Some(Side { id: "u-1".into(), offset: 42 }));
        l.set(Agent::Codex, Some(Side { id: "t-9".into(), offset: 7 }));
        l.save(&d, 5).unwrap();
        assert_eq!(Ledger::load(&d, 5), l);
        assert_eq!(Ledger::load(&d, 6), Ledger::default(), "a pane never switched has none");
    }

    #[test]
    fn the_watermark_belongs_to_one_session() {
        let mut l = Ledger::default();
        l.set(Agent::Claude, Some(Side { id: "u-1".into(), offset: 42 }));
        assert_eq!(l.watermark(Agent::Claude, "u-1"), 42);
        assert_eq!(l.watermark(Agent::Claude, "u-2"), 0, "another session starts from the top");
        assert_eq!(l.watermark(Agent::Codex, "u-1"), 0);
    }

    #[test]
    fn closed_panes_leave_nothing_behind() {
        let d = tmp("prune");
        for sid in [1u64, 2, 3] {
            std::fs::write(d.join(format!("{sid}.ledger")), "").unwrap();
            std::fs::write(d.join(format!("{sid}.md")), "").unwrap();
        }
        std::fs::write(d.join("2.tmp"), "").unwrap();
        std::fs::write(d.join("README"), "").unwrap();
        assert_eq!(prune(&d, &[1]), 5);
        let mut left: Vec<String> =
            std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
        left.sort();
        assert_eq!(left, vec!["1.ledger", "1.md", "README"], "only what it owns, only the dead");
    }
}
