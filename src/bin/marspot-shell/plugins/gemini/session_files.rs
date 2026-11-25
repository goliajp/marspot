//! Finding a gemini session's file: the project's chats directory, the
//! file of a given session id, and the newest file since a process
//! started.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use marspot_peer::json;

use super::pane::is_session_id;

/// `<config>/tmp/<project>/chats` for the project rooted at `cwd`.
pub(super) fn chats_dir(config_dir: &Path, cwd: &str) -> Option<PathBuf> {
    let text = std::fs::read_to_string(config_dir.join("projects.json")).ok()?;
    let registry = json::parse(&text).ok()?;
    let slug = registry.get("projects")?.str_at(cwd)?;
    // a path component out of a file another program writes
    let plain = !slug.is_empty() && slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    plain.then(|| config_dir.join("tmp").join(slug).join("chats"))
}

/// The main-session files in `chats`, with when each was last written.
/// Subagent sessions live in subdirectories and are not listed.
fn session_files(chats: &Path) -> Vec<(SystemTime, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(chats) else { return Vec::new() };
    rd.flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with("session-") && n.ends_with(".jsonl")))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file().then(|| meta.modified().ok().map(|m| (m, e.path()))).flatten()
        })
        .collect()
}

/// How much of a session file's first line to read.  The metadata
/// record is a few hundred bytes; a line that does not end inside this
/// is not one.
const HEAD_BYTES: u64 = 16 * 1024;

/// The session id a file opens with.
pub(crate) fn head_id(path: &Path) -> Option<String> {
    let mut buf = Vec::new();
    std::fs::File::open(path).ok()?.take(HEAD_BYTES).read_to_end(&mut buf).ok()?;
    let end = buf.iter().position(|b| *b == b'\n')?;
    let head = json::parse(std::str::from_utf8(&buf[..end]).ok()?).ok()?;
    head.str_at("sessionId").filter(|id| is_session_id(id)).map(str::to_string)
}

/// The file of session `id` in the project at `cwd`.
///
/// The name carries the id's first eight characters, which narrows the
/// directory to a file or two; the first line then has to name the
/// whole id.
pub(crate) fn by_id(config_dir: &Path, cwd: &str, id: &str) -> Option<PathBuf> {
    let suffix = format!("-{}.jsonl", id.get(..8).unwrap_or(id));
    session_files(&chats_dir(config_dir, cwd)?)
        .into_iter()
        .map(|(_, p)| p)
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(&suffix)))
        .find(|p| head_id(p).as_deref() == Some(id))
}

/// How many of the newest files are opened before giving up.
const NEWEST_TRIED: usize = 8;

/// The session written most recently since `started`, with its id.
/// Not a moment before: a file last written just before this process
/// started is the session of the gemini that was quit there a moment
/// earlier.
pub(super) fn newest_since(config_dir: &Path, cwd: &str, started: SystemTime) -> Option<(PathBuf, String)> {
    let mut files = session_files(&chats_dir(config_dir, cwd)?);
    files.retain(|(written, _)| *written >= started);
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    files.into_iter().take(NEWEST_TRIED).find_map(|(_, p)| head_id(&p).map(|id| (p, id)))
}
