//! Sessions as kimi writes them, for the tests.

use std::path::{Path, PathBuf};

/// Records a 2.5.3 build wrote against a local model server, with
/// the lines over 2 KB (tool and profile snapshots) left out.
pub(crate) const CANCELLED_THEN_COMPLETED: &str = include_str!("fixtures/wire-cancelled-then-completed.jsonl");
pub(crate) const STOP_HOOK: &str = include_str!("fixtures/wire-stop-hook.jsonl");
pub(crate) const TOOL_CALL: &str = include_str!("fixtures/wire-tool-call.jsonl");

pub(crate) const ID: &str = "session_84eb6e0a-2c78-40a4-b453-dc017d464a35";

/// A home holding session `id` with `wire` as its record, locked
/// by `pid`.  Returns the record's path.
pub(crate) fn session(home: &Path, id: &str, wire: &str, pid: Option<i32>) -> PathBuf {
    let dir = home.join("sessions/wd_a_0123456789ab").join(id);
    std::fs::create_dir_all(dir.join("agents/main")).unwrap();
    std::fs::write(dir.join("agents/main/wire.jsonl"), wire).unwrap();
    let line = format!("{{\"sessionId\":\"{id}\",\"sessionDir\":\"/elsewhere/sessions/wd_a_0123456789ab/{id}\",\"workDir\":\"/w/a\"}}\n");
    let mut index = std::fs::read_to_string(home.join("session_index.jsonl")).unwrap_or_default();
    index.push_str(&line);
    std::fs::write(home.join("session_index.jsonl"), index).unwrap();
    if let Some(pid) = pid {
        std::fs::create_dir_all(home.join("locks/sessions")).unwrap();
        let lock = format!(
            "{{\"version\":1,\"token\":\"{pid}:0d9f6c1e-5a55-4c3e-9a41-7f3d0c8f5b21\",\"pid\":{pid},\"hostname\":\"host.local\",\"startedAt\":1794351557245,\"clientIdentity\":\"kimi_code_cli\"}}"
        );
        std::fs::write(home.join("locks/sessions").join(format!("{id}.lock")), lock).unwrap();
    }
    dir.join("agents/main/wire.jsonl")
}
