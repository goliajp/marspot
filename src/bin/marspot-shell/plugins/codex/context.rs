//! How large a thread's context is, as its rollout says: found by the
//! thread's id, and read on from where the last read stopped.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use marspot_peer::json::{self, Value};

use crate::plugins::handoff::history::codex_rollout;
use crate::plugins::helpers::used::Used;

/// How much of a rollout's end is read at first: a thread gone on with
/// can have hundreds of megabytes before it.
const FIRST_BYTES: u64 = 256 * 1024;
/// How much is read in one go.
const READ_BYTES: u64 = 1024 * 1024;
/// How often a rollout that is not there yet is looked for.
const SEARCH_EVERY: Duration = Duration::from_secs(20);

/// Take in one record.
fn note(used: &mut Used, line: &str) {
    if line.contains(r#""type":"token_count""#) {
        let Ok(v) = json::parse(line) else { return };
        if v.at(&["payload", "type"]).and_then(Value::str) == Some("token_count") {
            let total = v.at(&["payload", "info", "last_token_usage", "total_tokens"]).and_then(Value::i64);
            used.tokens = total.and_then(|n| u64::try_from(n).ok()).filter(|n| *n > 0).or(used.tokens);
        }
    } else if used.had_tool != Some(true) && line.contains(r#"_call","#) && is_tool_call(line) {
        used.had_tool = Some(true);
    }
}

fn is_tool_call(line: &str) -> bool {
    let Ok(v) = json::parse(line) else { return false };
    v.str_at("type") == Some("response_item") && v.at(&["payload", "type"]).and_then(Value::str).is_some_and(|t| t.ends_with("_call"))
}

struct Rollout {
    path: PathBuf,
    /// Its size and write time when it was last read, and that the read
    /// did not reach its end.
    read: Option<(u64, SystemTime)>,
    behind: bool,
    /// How far `used` reaches.
    at: u64,
    /// `at` is inside a record.
    mid_record: bool,
    used: Used,
}

impl Rollout {
    fn new(path: PathBuf) -> Self {
        Self { path, read: None, behind: false, at: 0, mid_record: false, used: Used::default() }
    }

    /// Read what was written since.  None when it cannot be read.
    fn read_on(&mut self) -> Option<()> {
        let md = std::fs::metadata(&self.path).ok()?;
        let now = (md.len(), md.modified().ok()?);
        if self.read == Some(now) && !self.behind {
            return Some(());
        }
        if self.read.is_none() || now.0 < self.at {
            self.at = now.0.saturating_sub(FIRST_BYTES);
            self.mid_record = self.at > 0;
            self.used.had_tool = (self.at == 0).then_some(false);
        }
        let mut f = std::fs::File::open(&self.path).ok()?;
        f.seek(SeekFrom::Start(self.at)).ok()?;
        let mut buf = Vec::new();
        f.take(READ_BYTES).read_to_end(&mut buf).ok()?;
        let full = buf.len() as u64 == READ_BYTES;
        if let Some(end) = buf.iter().rposition(|b| *b == b'\n') {
            let text = String::from_utf8_lossy(&buf[..end]);
            let skip = usize::from(std::mem::take(&mut self.mid_record));
            text.split('\n').skip(skip).for_each(|l| note(&mut self.used, l));
            self.at += end as u64 + 1;
        } else if full {
            // a record larger than one read: no count is
            self.at += READ_BYTES;
            self.mid_record = true;
        }
        self.read = Some(now);
        self.behind = full;
        Some(())
    }
}

struct Followed {
    home: PathBuf,
    thread: String,
    searched: Instant,
    rollout: Option<Rollout>,
}

/// The rollouts followed, by pane.
#[derive(Default)]
pub(in crate::plugins) struct Contexts(HashMap<u64, Followed>);

impl Contexts {
    pub fn retain(&mut self, keep: impl Fn(u64) -> bool) {
        self.0.retain(|sid, _| keep(*sid));
    }

    /// What the rollout of `thread`, which pane `sid` is in, says.
    pub fn look(&mut self, sid: u64, home: &Path, thread: &str, now: Instant) -> Option<Used> {
        let same = self.0.get(&sid).filter(|f| f.home == home && f.thread == thread);
        let search = same.is_none_or(|f| f.rollout.is_none() && now.saturating_duration_since(f.searched) >= SEARCH_EVERY);
        if search {
            let rollout = codex_rollout(home, thread).map(Rollout::new);
            self.0.insert(sid, Followed { home: home.to_path_buf(), thread: thread.to_string(), searched: now, rollout });
        }
        let f = self.0.get_mut(&sid)?;
        let r = f.rollout.as_mut()?;
        if r.read_on().is_none() {
            f.rollout = None;
            return None;
        }
        Some(r.used)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::handoff::history::tmp;
    use std::io::Write;

    const ID: &str = "01a20b03-d477-7222-a70e-cab26b183b4b";
    const META: &str = r#"{"type":"session_meta","payload":{"id":"x"}}"#;

    fn count(total: u64) -> String {
        format!(r#"{{"timestamp":"t","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"total_tokens":999999}},"last_token_usage":{{"input_tokens":1,"total_tokens":{total}}}}}}}}}"#)
    }

    fn append(path: &Path, text: &str) {
        std::fs::OpenOptions::new().append(true).create(true).open(path).unwrap().write_all(text.as_bytes()).unwrap();
    }

    fn rollout(home: &Path) -> PathBuf {
        let day = home.join("sessions/2026/11/24");
        std::fs::create_dir_all(&day).unwrap();
        day.join(format!("rollout-2026-11-24T09-45-30-{ID}.jsonl"))
    }

    #[test]
    fn codex_context_is_the_last_count_of_the_threads_own_rollout() {
        let home = tmp("codex-context");
        let path = rollout(&home);
        let mut c = Contexts::default();
        let t = Instant::now();
        assert_eq!(c.look(7, &home, ID, t), None);
        append(&path, &format!("{META}\n{}\n", count(23_183)));
        assert_eq!(c.look(7, &home, ID, t), None, "not looked for again at once");
        let t = t + SEARCH_EVERY;
        let used = |tokens, had_tool| Some(Used { tokens: Some(tokens), had_tool: Some(had_tool) });
        assert_eq!(c.look(7, &home, ID, t), used(23_183, false));
        // a model that writes about it calls nothing
        append(&path, "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"text\":\"a \\\"x_call\\\", \"}}\n");
        // no count yet, and a record still being written
        append(&path, "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":null}}\n");
        append(&path, &count(30_000));
        assert_eq!(c.look(7, &home, ID, t), used(23_183, false));
        append(&path, "\n{\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call\",\"name\":\"exec\"}}\n");
        assert_eq!(c.look(7, &home, ID, t), used(30_000, true));
        // another thread in the pane
        assert_eq!(c.look(7, &home, "01a20b03-d477-7222-a70e-000000000000", t), None);
        c.retain(|_| false);
        assert!(c.0.is_empty());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn codex_context_reads_the_end_at_first_and_then_what_was_added() {
        let home = tmp("codex-context-long");
        let path = rollout(&home);
        let filler = format!("{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"text\":\"{}\"}}}}\n", "x".repeat(1000));
        append(&path, &format!("{}\n{}{}\n", count(1), filler.repeat(400), count(2)));
        let mut c = Contexts::default();
        let t = Instant::now();
        assert_eq!(c.look(7, &home, ID, t), Some(Used { tokens: Some(2), had_tool: None }));
        let reached = c.0[&7].rollout.as_ref().unwrap().at;
        assert_eq!(reached, std::fs::metadata(&path).unwrap().len());
        // more than one read: on the next looks
        append(&path, &format!("{}{}\n", filler.repeat(1500), count(3)));
        assert_eq!(c.look(7, &home, ID, t).unwrap().tokens, Some(2));
        assert_eq!(c.look(7, &home, ID, t).unwrap().tokens, Some(3));
        // a record longer than a read
        append(&path, &format!("{{\"x\":\"{}\"}}\n{}\n", "y".repeat(2 * READ_BYTES as usize), count(4)));
        for _ in 0..3 {
            c.look(7, &home, ID, t);
        }
        assert_eq!(c.look(7, &home, ID, t).unwrap().tokens, Some(4));
        std::fs::remove_dir_all(&home).unwrap();
    }
}
