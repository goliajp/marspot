//! Reading claude's jsonl transcripts: what a record is, what the
//! session was last doing, and whether there is a conversation in it.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use super::CcActivity;

/// How much of a transcript's end the cheap checks read.  A single
/// record longer than this (a huge tool result) reads as `Unknown`,
/// which the callers already treat as "in flight".
const TAIL: u64 = 32 * 1024;

/// Value of a **top-level** string field of one jsonl record.
///
/// Depth-aware, and that is the whole point.  Measured off live
/// records, the field order is not what a reader assumes:
///
/// ```text
/// user:      {"parentUuid":…,"promptId":…,"type":"user","message":{…}}
/// assistant: {"parentUuid":…,"message":{"model":…,"type":"message",…},…,"type":"assistant",…}
/// ```
///
/// Assistant records put `message` — which has its own
/// `"type":"message"` — **before** their own `type`, so "first
/// `"type"` in the line" reads every assistant record as `message`.
/// That shipped in 0.7.38 and made all 9 live panes report `working`:
/// assistant records were being skipped as bookkeeping and the scan
/// fell through to the user record behind them.  Scanning at depth 1
/// is the fix, and it retires the whole class of field-order bugs.
pub(super) fn top_level_str<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    let b = line.as_bytes();
    // Content of the JSON string starting at `at` (which must be the
    // opening quote), plus the index just past its closing quote.
    // Escapes are stepped over, not decoded — the values this reads
    // ("assistant", "user", …) have none.
    fn scan_string(b: &[u8], at: usize) -> Option<(&str, usize)> {
        let mut i = at + 1;
        let start = i;
        while i < b.len() {
            match b[i] {
                b'\\' => i += 2,
                b'"' => {
                    return std::str::from_utf8(&b[start..i]).ok().map(|s| (s, i + 1))
                }
                _ => i += 1,
            }
        }
        None
    }
    let mut i = 0usize;
    let mut depth: i32 = 0;
    while i < b.len() {
        match b[i] {
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                i += 1;
            }
            b'"' => {
                let (s, after) = scan_string(b, i)?;
                let mut j = after;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                // A key is a string followed by ':'.  Only depth 1 is
                // the record's own object.
                if depth == 1 && j < b.len() && b[j] == b':' {
                    let mut k = j + 1;
                    while k < b.len() && b[k].is_ascii_whitespace() {
                        k += 1;
                    }
                    if s == field {
                        return if k < b.len() && b[k] == b'"' {
                            scan_string(b, k).map(|(v, _)| v)
                        } else {
                            None // present but not a string
                        };
                    }
                    i = k; // resume at the value; its braces adjust depth
                } else {
                    i = after;
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// The record's own `type` — `assistant`, `user`, `system`,
/// `last-prompt`, `attachment`, `ai-title`, `mode`, `permission-mode`,
/// … (that list is what live transcripts actually contain).
pub(super) fn record_type(line: &str) -> Option<&str> {
    top_level_str(line, "type")
}

/// Classify one conversation record.  Split from the file read so the
/// shape rules are testable against literal records.
///
/// `has_young_child` answers "does claude have a process younger than
/// this record" — the caller resolves it from the proc table it already
/// walked, since a tool that shells out shows up as a child of claude
/// while an approval prompt shows nothing.
pub(super) fn activity_from_last_record(line: &str, has_young_child: bool) -> CcActivity {
    match record_type(line) {
        Some("assistant") => {
            if line.contains("\"type\":\"tool_use\"") {
                CcActivity::ToolPending { executing: has_young_child }
            } else {
                CcActivity::AwaitingUser
            }
        }
        // A user record is either a real prompt or the transcript's
        // record of a tool result; both leave the assistant owing the
        // next record.
        Some("user") => CcActivity::Working,
        _ => CcActivity::Unknown,
    }
}

/// True for records that say nothing about whose turn it is and must be
/// skipped when scanning back for the conversation's last state.
///
/// This is not a hypothetical: claudecode writes
/// `{"type":"system","subtype":"turn_duration",…}` **after** the
/// assistant's closing message, so the single most common resting state
/// — a finished turn waiting on the user — sits behind one of these.
/// Classifying only the literal last line read `unknown` on 7 of 9 live
/// panes, which is what caught it.
pub(super) fn is_bookkeeping_record(line: &str) -> bool {
    !matches!(record_type(line), Some("assistant") | Some("user"))
}

/// Read the first line of `path` and try to parse `"sessionId":"<uuid>"`
/// out of it.  Cheap regex-free string scan — JSON parser would be
/// overkill for one well-known field, and pulling serde_json into the
/// shell binary just for this would violate the self-build principle.
pub(super) fn parse_session_id(path: &PathBuf) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let f = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(f);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    // claudecode jsonl always starts the file with a "mode" record
    // containing `"sessionId":"<uuid>"`.  We look for that key.
    let key = "\"sessionId\":\"";
    let i = line.find(key)? + key.len();
    let rest = &line[i..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Cheap "what's the type of the last record" check.  Reads at most
/// the last 32 KiB of the file and looks at the last `\n`-separated
/// record's `"type":"…"` field.  Returns None if the file is malformed
/// or has no recognisable type.
/// The last [`TAIL`] bytes of a transcript as text.  None when it
/// cannot be read.
fn read_tail(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(path).ok()?;
    let start = f.metadata().ok()?.len().saturating_sub(TAIL);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity(TAIL as usize);
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Transcript tails, read once per file version.
///
/// A scan asks each bound transcript what the session is doing, whether
/// it waits on its own timer and whether its account refused it; all
/// three come from the same window, and a resting session's file does
/// not change between scans, so the window is read again only when the
/// file's (size, mtime) moved.  Bounded by the transcripts the scan
/// keeps in `seen`.
#[derive(Default)]
pub(super) struct TailCache {
    map: HashMap<PathBuf, (u64, SystemTime, Arc<str>)>,
    pub(super) reads: u64,
}

impl TailCache {
    pub(super) fn tail(&mut self, path: &Path) -> Option<Arc<str>> {
        let md = fs::metadata(path).ok()?;
        let key = (md.len(), md.modified().ok()?);
        if let Some((len, mtime, text)) = self.map.get(path)
            && (*len, *mtime) == key
        {
            return Some(text.clone());
        }
        self.reads += 1;
        let text: Arc<str> = read_tail(path)?.into();
        self.map.insert(path.to_path_buf(), (key.0, key.1, text.clone()));
        Some(text)
    }

    pub(super) fn retain(&mut self, keep: impl Fn(&PathBuf) -> bool) {
        self.map.retain(|p, _| keep(p));
    }
}

/// The tail window of a transcript as text, for the cheap
/// "what has this session been doing" checks; a read failure yields an
/// empty string, which the callers read as "no evidence" — and every
/// caller's no-evidence answer is the conservative one.
#[cfg(test)]
pub(super) fn tail_window(path: &Path) -> String {
    read_tail(path).unwrap_or_default()
}

/// Read a session's last record and classify it.
#[cfg(test)]
pub(super) fn tail_activity(path: &Path, has_young_child: bool) -> CcActivity {
    activity_of(read_tail(path).as_deref(), has_young_child)
}

/// Classify a session by the last conversation record in its tail
/// window; `Unknown` when there is no window or no such record in it.
pub(super) fn activity_of(tail: Option<&str>, has_young_child: bool) -> CcActivity {
    let Some(s) = tail else {
        return CcActivity::Unknown;
    };
    // Walk back to the last record that is part of the conversation,
    // stepping over the bookkeeping ones claudecode appends after a
    // turn.  Bounded so a long run of them can't turn one tick into a
    // scan of the whole window.
    const MAX_SKIP: usize = 64;
    match s
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(MAX_SKIP)
        .find(|l| !is_bookkeeping_record(l))
    {
        Some(last) => activity_from_last_record(last, has_young_child),
        None => CcActivity::Unknown,
    }
}

pub(super) fn tail_last_message_type(path: &PathBuf) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 32 * 1024;
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity(TAIL as usize);
    f.read_to_end(&mut buf).ok()?;
    // Find the last newline-anchored record.
    let s = String::from_utf8_lossy(&buf);
    let last_line = s.lines().rev().find(|l| !l.trim().is_empty())?;
    let key = "\"type\":\"";
    let i = last_line.find(key)? + key.len();
    let rest = &last_line[i..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Whether the transcript holds a conversation: at least one `user` or
/// `assistant` record.
///
/// claude can leave a file behind that names a session and nothing
/// else -- an `ai-title` and an `agent-name` record, 200 bytes -- and
/// `claude --resume` on it answers "No conversation found" and exits.
/// One of these was bound to a pane that was in fact writing another
/// session, and a profile switch tried to resume it.
///
/// Reads from the start and stops at the first conversation record.
/// Transcripts are append-only, so once true the answer never changes
/// and callers cache it; only files without a conversation, which are
/// the small ones, are read to the end.
pub(super) fn has_conversation(path: &PathBuf) -> bool {
    use std::io::{BufRead, BufReader};
    let Ok(f) = fs::File::open(path) else {
        return false;
    };
    let mut reader = BufReader::new(f);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return false,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line);
        if matches!(record_type(&text), Some("user") | Some("assistant")) {
            return true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(name: &str, body: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "marspot-transcript-{}-{name}.jsonl",
            std::process::id()
        ));
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn a_title_only_transcript_has_no_conversation() {
        // the file that was resumed, byte for byte apart from the uuid
        let p = write(
            "title-only",
            "{\"type\":\"ai-title\",\"aiTitle\":\"继续 autorun\",\"sessionId\":\"e46ca782\"}\n\
             {\"type\":\"agent-name\",\"agentName\":\"继续 autorun\",\"sessionId\":\"e46ca782\"}\n",
        );
        assert!(!has_conversation(&p));
        fs::remove_file(&p).unwrap();
    }

    #[test]
    fn a_user_record_after_bookkeeping_is_a_conversation() {
        let p = write(
            "user",
            "{\"type\":\"mode\",\"mode\":\"normal\",\"sessionId\":\"s\"}\n\
             {\"type\":\"attachment\",\"attachment\":{\"type\":\"user\"}}\n\
             {\"parentUuid\":null,\"type\":\"user\",\"message\":{\"role\":\"user\"}}\n",
        );
        assert!(has_conversation(&p));
        fs::remove_file(&p).unwrap();
    }

    #[test]
    fn a_nested_user_type_is_not_a_conversation() {
        // only the record's own type counts, not one inside an attachment
        let p = write(
            "nested",
            "{\"type\":\"attachment\",\"attachment\":{\"type\":\"user\"}}\n",
        );
        assert!(!has_conversation(&p));
        fs::remove_file(&p).unwrap();
    }

    /// What the session is doing and what its window says come from
    /// one read, and an unchanged file is not read again.
    #[test]
    fn a_tail_is_read_once_per_file_version() {
        let p = write(
            "tail-once",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\"}]}}\n",
        );
        let mut cache = TailCache::default();
        for _ in 0..3 {
            let tail = cache.tail(&p);
            assert_eq!(activity_of(tail.as_deref(), false), CcActivity::AwaitingUser);
            assert!(tail.as_deref().is_some_and(|w| w.contains("assistant")));
        }
        assert_eq!(cache.reads, 1, "one read serves every scan of an unchanged file");
        let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
        std::io::Write::write_all(&mut f, b"{\"type\":\"user\"}\n").unwrap();
        assert_eq!(activity_of(cache.tail(&p).as_deref(), false), CcActivity::Working);
        assert_eq!(cache.reads, 2, "a grown file is read again");
        cache.retain(|_| false);
        fs::remove_file(&p).unwrap();
        assert!(cache.tail(&p).is_none());
        assert_eq!(activity_of(None, false), CcActivity::Unknown);
    }

    #[test]
    fn a_missing_file_has_no_conversation() {
        assert!(!has_conversation(&PathBuf::from("/nonexistent/x.jsonl")));
    }
}
