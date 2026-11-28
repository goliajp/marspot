//! What each pane last ran codex as, kept on disk so a pane whose
//! session process started afresh can have its codex put back: the
//! thread, the home it ran under, its directory and model, and the
//! shell it ran under.  When a row goes and when it is owed a restore
//! is the same for every agent (`plugins::cold_restore`).

use crate::plugins::cold_restore::ColdRow;
use crate::plugins::{pidtree, pty_op};

use super::pane::is_thread_id;

/// Most rows kept; the oldest goes.  One per pane, and the session
/// population is capped well below this.
pub(super) const ROWS_CAP: usize = 128;

/// The last codex a pane ran, and the shell it ran under.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct LastThread {
    pub(super) sid: u64,
    pub(super) thread: String,
    /// The `CODEX_HOME` it ran under: its account.
    pub(super) home: String,
    pub(super) cwd: Option<String>,
    pub(super) model: Option<String>,
    pub(super) shell: pidtree::ProcId,
}

impl ColdRow for LastThread {
    fn sid(&self) -> u64 {
        self.sid
    }
    fn shell(&self) -> pidtree::ProcId {
        self.shell
    }
}

pub(super) fn encode(rows: &[LastThread]) -> String {
    let mut s = String::new();
    for r in rows {
        let opt = |v: &Option<String>| v.clone().unwrap_or_default();
        s.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            r.sid,
            r.thread,
            r.shell.pid,
            r.shell.start_us,
            r.home,
            opt(&r.cwd),
            opt(&r.model)
        ));
    }
    s
}

/// The rows of `text`; a row with a field that could not go on a
/// command line is dropped, since every field ends up on one.
pub(super) fn decode(text: &str) -> Vec<LastThread> {
    let mut rows = Vec::new();
    for row in text.lines().filter_map(decode_row) {
        // a thread is one pane's: the later row is the newer binding
        rows.retain(|r: &LastThread| r.thread != row.thread && r.sid != row.sid);
        rows.push(row);
    }
    let over = rows.len().saturating_sub(ROWS_CAP);
    rows.drain(..over);
    rows
}

fn decode_row(line: &str) -> Option<LastThread> {
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() != 7 {
        return None;
    }
    let some = |v: &str| (!v.is_empty()).then(|| v.to_string());
    let shell = pidtree::ProcId { pid: f[2].parse().ok()?, start_us: f[3].parse().ok()? };
    row_of(f[0].parse().ok()?, f[1], f[4], some(f[5]), some(f[6]), shell)
}

fn thread_chars_ok(t: &str) -> bool {
    t.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// A model name that goes on the line as one word.
pub(super) fn model_ok(m: &str) -> bool {
    m.len() <= 64 && m.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
}

/// The row a bound pane makes; None for one whose thread or home
/// could not go on a command line, since every field ends up on one.
pub(super) fn row_of(sid: u64, thread: &str, home: &str, cwd: Option<String>, model: Option<String>, shell: pidtree::ProcId) -> Option<LastThread> {
    let ok = is_thread_id(thread) && thread_chars_ok(thread) && !home.is_empty() && pty_op::shell_safe(home);
    ok.then(|| LastThread {
        sid,
        thread: thread.to_string(),
        home: home.to_string(),
        cwd: cwd.filter(|d| pty_op::quotable(d)),
        model: model.filter(|m| model_ok(m)),
        shell,
    })
}

/// Put `new` in place of the pane's row; true when anything changed.
/// A model already known for the same thread is kept while the rollout
/// has not said one yet.  Another pane's row for the same thread goes:
/// the thread moved here.
pub(super) fn upsert(rows: &mut Vec<LastThread>, mut new: LastThread) -> bool {
    let before = rows.len();
    rows.retain(|r| r.sid == new.sid || r.thread != new.thread);
    let moved = rows.len() != before;
    moved
        | match rows.iter().position(|r| r.sid == new.sid) {
            Some(i) => {
                if rows[i].thread == new.thread && new.model.is_none() {
                    new.model = rows[i].model.clone();
                }
                let changed = rows[i] != new;
                rows[i] = new;
                changed
            }
            None => {
                if rows.len() >= ROWS_CAP {
                    rows.remove(0);
                }
                rows.push(new);
                true
            }
        }
}

/// The thread a codex was started to resume, from its argv
/// (`codex resume <id>`); None for a fresh one, or `--last`.
pub(super) fn argv_thread(argv: &[String]) -> Option<&str> {
    let at = argv.iter().position(|w| w == "resume")?;
    argv[at + 1..].iter().find(|w| !w.starts_with('-')).map(String::as_str).filter(|w| is_thread_id(w))
}

#[cfg(test)]
#[path = "cold_record_tests.rs"]
mod tests;
