//! Following a conversation that was carried on in another session.
//!
//! Sending a conversation to the background, or forking it, ends the
//! old session's transcript with a `continued-in` record naming the
//! session that carries it on. The old id still resumes -- into the
//! conversation as it stood at that moment. On 2026-11-21 a pane whose
//! conversation had gone to a background session the evening before
//! was moved to another account by its old id, and a day of work
//! vanished from the pane: still on disk, in the session it had
//! continued in.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::statusline::json_string_field;

/// How much of a transcript's end is read. The record is written last,
/// after nothing but a few lines of session metadata.
const TAIL: u64 = 1 << 20;
/// A conversation sent on and on; a cycle in the records ends here.
const HOPS: usize = 8;

/// The session that now carries `uuid`'s conversation, among the
/// transcripts in `project`: `uuid` itself unless its transcript ends
/// by naming another that exists, followed to the last.
pub(super) fn latest(project: &Path, uuid: &str) -> String {
    let mut at = uuid.to_string();
    for _ in 0..HOPS {
        match continued_in(&project.join(format!("{at}.jsonl"))) {
            Some(next) if next != at && project.join(format!("{next}.jsonl")).is_file() => at = next,
            _ => break,
        }
    }
    at
}

/// The session a transcript says it continued in, when no turn was
/// taken in it since: one resumed and worked in again is its own
/// conversation once more.
fn continued_in(path: &Path) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let mut next = None;
    for line in String::from_utf8_lossy(&buf).lines() {
        if line.starts_with("{\"type\":\"continued-in\"") {
            next = json_string_field(line, "\"continuedInSessionId\":\"");
        } else if line.contains("\"type\":\"user\"") || line.contains("\"type\":\"assistant\"") {
            next = None;
        }
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "32f6eecf-5377-400d-958d-1ca24f1821d7";
    const BG: &str = "0a76d287-c435-4cc2-b9e7-a68ffca225db";

    fn turn(kind: &str, sid: &str) -> String {
        format!("{{\"parentUuid\":null,\"type\":\"{kind}\",\"message\":{{}},\"sessionId\":\"{sid}\"}}\n")
    }

    /// The end of the old transcript as the CLI wrote it on 2026-11-20.
    fn sent_on(sid: &str, to: &str) -> String {
        format!(
            "{}{{\"type\":\"continued-in\",\"timestamp\":\"2026-11-20T15:00:36.946Z\",\"sessionId\":\"{sid}\",\"continuedInSessionId\":\"{to}\"}}\n\
             {{\"type\":\"last-prompt\",\"lastPrompt\":\"x\",\"sessionId\":\"{sid}\"}}\n",
            turn("user", sid)
        )
    }

    fn project(files: &[(&str, String)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cont-{}", marspot_peer::rand::hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        for (id, body) in files {
            std::fs::write(dir.join(format!("{id}.jsonl")), body).unwrap();
        }
        dir
    }

    #[test]
    fn a_conversation_sent_to_the_background_is_found_where_it_went() {
        let dir = project(&[(OLD, sent_on(OLD, BG)), (BG, turn("assistant", BG))]);
        assert_eq!(latest(&dir, OLD), BG);
        assert_eq!(latest(&dir, BG), BG, "the newest is its own");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_session_worked_in_again_after_it_was_sent_on_is_its_own() {
        let body = sent_on(OLD, BG) + &turn("user", OLD);
        let dir = project(&[(OLD, body), (BG, turn("assistant", BG))]);
        assert_eq!(latest(&dir, OLD), OLD);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_continuation_with_no_transcript_or_a_loop_stops_where_it_can() {
        let dir = project(&[(OLD, sent_on(OLD, BG))]);
        assert_eq!(latest(&dir, OLD), OLD, "nothing to resume there");
        std::fs::write(dir.join(format!("{BG}.jsonl")), sent_on(BG, OLD)).unwrap();
        let end = latest(&dir, OLD);
        assert!(end == OLD || end == BG, "a loop ends: {end}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
