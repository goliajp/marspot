//! What gemini writes into a session file, for tests.

use std::path::{Path, PathBuf};

/// The opening record, as a v0.67 build wrote it for a session
/// started with `--session-id`.
pub(crate) fn head(id: &str) -> String {
    format!(
        r#"{{"sessionId":"{id}","projectHash":"1a2fd07c4d49eacee98ce19e4fb41321035bb110963bb85856985c06736f09a4","startTime":"2026-11-10T06:10:20.221Z","lastUpdated":"2026-11-10T06:10:20.221Z","kind":"main"}}"#
    ) + "\n"
}

/// The context gemini records as a user message before the first
/// real one.
pub(crate) fn context() -> String {
    r#"{"id":"c0","timestamp":"2026-11-10T06:10:20.221Z","type":"user","content":[{"text":"<session_context>\nThis is the Gemini CLI.\n</session_context>"}]}"#.to_string()
        + "\n"
        + &touched()
}

/// The `lastUpdated` record gemini appends after every message.
pub(crate) fn touched() -> String {
    r#"{"$set":{"lastUpdated":"2026-11-10T06:10:20.222Z"}}"#.to_string() + "\n"
}

pub(crate) fn user(id: &str, text: &str) -> String {
    format!(r#"{{"id":"{id}","timestamp":"2026-11-10T06:11:00.000Z","type":"user","content":[{{"text":"{text}"}}]}}"#)
        + "\n"
        + &touched()
}

pub(crate) fn reply(id: &str, text: &str, model: &str) -> String {
    format!(
        r#"{{"id":"{id}","timestamp":"2026-11-10T06:11:02.000Z","type":"gemini","content":"{text}","thoughts":[],"tokens":{{"input":1,"output":1,"cached":0,"thoughts":0,"tool":0,"total":2}},"model":"{model}"}}"#
    ) + "\n"
        + &touched()
}

/// What the interface records beside the conversation: a line it
/// printed, such as the answer to `/model set`.
pub(crate) fn info(id: &str, text: &str) -> String {
    format!(r#"{{"id":"{id}","timestamp":"2026-11-10T06:11:04.000Z","type":"info","content":"{text}"}}"#) + "\n" + &touched()
}

/// A config directory whose registry maps `cwd` to `slug`; returns
/// the project's chats directory.
pub(crate) fn project(config_dir: &Path, cwd: &str, slug: &str) -> PathBuf {
    let chats = config_dir.join("tmp").join(slug).join("chats");
    std::fs::create_dir_all(&chats).unwrap();
    std::fs::write(
        config_dir.join("projects.json"),
        format!("{{\n  \"projects\": {{\n    \"{cwd}\": \"{slug}\"\n  }}\n}}"),
    )
    .unwrap();
    chats
}

/// The name gemini gives a session's file.
pub(crate) fn file_name(id: &str) -> String {
    format!("session-2026-11-10T06-10-{}.jsonl", &id[..8])
}

/// A line the interface printed as an error, such as a turn stopped by
/// a quota error, at `at` (`2026-11-10T06:11:06.000Z` form).
pub(crate) fn error_at(id: &str, at: &str, text: &str) -> String {
    let text = text.replace('\\', "\\\\").replace('"', "\\\"");
    format!(r#"{{"id":"{id}","timestamp":"{at}","type":"error","content":"{text}"}}"#) + "\n" + &touched()
}

/// `info` at a time of the test's choosing.
pub(crate) fn info_at(id: &str, at: &str, text: &str) -> String {
    format!(r#"{{"id":"{id}","timestamp":"{at}","type":"info","content":"{text}"}}"#) + "\n" + &touched()
}

/// `reply` at a time of the test's choosing.
pub(crate) fn reply_at(id: &str, at: &str, model: &str) -> String {
    format!(
        r#"{{"id":"{id}","timestamp":"{at}","type":"gemini","content":"ok","thoughts":[],"tokens":{{"input":1,"output":1,"cached":0,"thoughts":0,"tool":0,"total":2}},"model":"{model}"}}"#
    ) + "\n"
        + &touched()
}
