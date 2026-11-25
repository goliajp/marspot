//! The end of a transcript, read far enough back to hold its last
//! conversation record whole.
//!
//! Claude Code can write a bookkeeping record of 100 KiB and more after
//! a turn's last message: an `attachment` with the whole system prompt
//! in it, between the closing `assistant` record and its
//! `turn_duration`.  A window of the usual size then starts inside that
//! record and holds no conversation record at all: the session reads
//! `Unknown` and its finished turn as not over.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::is_bookkeeping_record;

/// The most that is read back from a transcript's end.
pub(in crate::plugins::claudecode) const MOST: u64 = 2 * 1024 * 1024;

/// The end of the file at `path`, `len` bytes long, and where it starts:
/// the last `from` bytes, then `factor` times as many each time, until
/// `done` says what was read is enough, the whole file is read or
/// [`MOST`] is.  `done` is given the bytes and where they start.
pub(in crate::plugins::claudecode) fn read_back(
    path: &Path,
    len: u64,
    from: u64,
    factor: u64,
    mut done: impl FnMut(&[u8], u64) -> bool,
) -> Option<(Vec<u8>, u64)> {
    let mut f = File::open(path).ok()?;
    let mut window = from;
    loop {
        let start = len.saturating_sub(window);
        f.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = Vec::with_capacity((len - start) as usize);
        f.by_ref().take(len - start).read_to_end(&mut buf).ok()?;
        if done(&buf, start) || start == 0 || window >= MOST {
            return Some((buf, start));
        }
        window = window.saturating_mul(factor).min(MOST);
    }
}

/// What was read of a transcript's end.
pub(super) struct Reach {
    pub(super) text: String,
    /// How many bytes back from the end the text, or the search for a
    /// conversation record, reached.
    pub(super) window: u64,
    /// Bytes taken from the file.
    pub(super) read: u64,
    /// Set when no conversation record was found as far back as the
    /// search may go: where the last record read starts.  The text is
    /// then only the last `first` bytes, and a later read of the grown
    /// file need only look from there on ([`grown`]).
    pub(super) bare: Option<u64>,
}

/// The end of the transcript at `path`, `len` bytes long: the last
/// `first` bytes, then four times as many each time, until they hold a
/// whole conversation record, the whole file or [`MOST`].
pub(super) fn read(path: &Path, len: u64, first: u64) -> Option<Reach> {
    let mut found = false;
    let (buf, start) = read_back(path, len, first, 4, |b, start| {
        found = holds_conversation(b, start > 0);
        found
    })?;
    let read = buf.len() as u64;
    if found {
        return Some(Reach { text: String::from_utf8_lossy(&buf).into_owned(), window: len - start, read, bare: None });
    }
    // a record longer than all that was read gives no place to go on from
    let last = buf.iter().rposition(|&b| b == b'\n');
    let bare = last.map(|i| start + i as u64 + 1).or((start == 0).then_some(0));
    Some(Reach { text: last_of(&buf, first), window: len - start, read, bare })
}

/// The end of a transcript that has grown to `len` bytes since `was`
/// found no conversation record: only what came after the last record it
/// read is looked at, unless that holds one.
pub(super) fn grown(path: &Path, len: u64, first: u64, was: &Reach, was_len: u64) -> Option<Reach> {
    let Some(clean) = was.bare.filter(|c| len > was_len && *c <= was_len && len - c <= MOST) else {
        return read(path, len, first);
    };
    let start = clean.min(len.saturating_sub(first));
    let (buf, at) = read_back(path, len, len - start, 1, |_, _| true)?;
    let new = buf.get((clean - at) as usize..).unwrap_or_default();
    if holds_conversation(new, false) {
        return read(path, len, first);
    }
    let clean = new.iter().rposition(|&b| b == b'\n').map_or(clean, |i| clean + i as u64 + 1);
    let window = was.window.saturating_add(len - was_len);
    Some(Reach { text: last_of(&buf, first), window, read: buf.len() as u64, bare: Some(clean) })
}

/// The last `n` bytes of `buf` as text.
fn last_of(buf: &[u8], n: u64) -> String {
    let keep = buf.len().saturating_sub(n as usize);
    String::from_utf8_lossy(&buf[keep..]).into_owned()
}

/// Whether `bytes` hold a whole record of the conversation; `cut`: they
/// start inside a record.
fn holds_conversation(bytes: &[u8], cut: bool) -> bool {
    let mut lines = bytes.split(|&b| b == b'\n').skip(usize::from(cut));
    lines.any(|l| {
        let l = String::from_utf8_lossy(l);
        !l.trim().is_empty() && !is_bookkeeping_record(&l)
    })
}

#[cfg(test)]
mod tests {
    use marspot::pane_state::Activity;

    use super::super::{TAIL, TailCache, activity_of};
    use crate::plugins::claudecode::with_helpers::turns::{Look, look_in};

    fn len_of(path: &std::path::Path) -> u64 {
        std::fs::metadata(path).unwrap().len()
    }

    /// A turn answered without a tool and no result file, then a
    /// prompt-snapshot attachment of 128 KiB, the stop-hook summary, the
    /// turn's duration and two cost records: the first window starts
    /// inside the attachment.
    #[test]
    fn helpers_a_turn_end_behind_a_long_record_is_seen() {
        let earlier = format!(r#"{{"type":"user","message":{{"role":"user","content":"{}"}}}}"#, "e".repeat(2048));
        let asked = r#"{"type":"user","message":{"role":"user","content":"Reply with hello"},"timestamp":"2026-11-25T20:54:20.411Z"}"#;
        let said = r#"{"message":{"role":"assistant","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn"},"type":"assistant","timestamp":"2026-11-25T20:54:23.933Z"}"#;
        let snapshot = format!(r#"{{"isSidechain":false,"attachment":{{"type":"prompt_snapshot","systemPrompt":["{}"]}},"type":"attachment","timestamp":"2026-11-25T20:54:23.958Z"}}"#, "x".repeat(128 * 1024));
        let hooks = r#"{"type":"system","subtype":"stop_hook_summary","timestamp":"2026-11-25T20:54:24.012Z"}"#;
        let over = r#"{"type":"system","subtype":"turn_duration","durationMs":3600,"timestamp":"2026-11-25T20:54:24.015Z"}"#;
        let after = r#"{"type":"cost-state","sessionId":"s"}"#;
        let mut lines: Vec<&str> = vec![earlier.as_str(); 400];
        lines.extend([asked, said, snapshot.as_str(), hooks, over, after, after]);
        let text = lines.join("\n") + "\n";
        let dir = std::env::temp_dir().join(format!("marspot-tail-reach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(&path, &text).unwrap();
        let r = super::read(&path, len_of(&path), TAIL).unwrap();
        let short = &text[text.len() - TAIL as usize..];
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(activity_of(Some(short), false), Activity::Unknown, "what the first window alone says");
        assert!(r.window > TAIL && r.window < text.len() as u64, "read back to the answer, not the whole file: {}", r.window);
        assert_eq!(r.bare, None);
        assert_eq!(activity_of(Some(&r.text), false), Activity::AwaitingUser);
        assert!(matches!(look_in("record", Some(&r.text)), Some(Look::Over { .. })), "its turn is over");
    }

    /// A transcript whose last 2 MiB are bookkeeping alone is read that
    /// far once; as it grows, only what it grew by is read, until a
    /// conversation record comes.
    #[test]
    fn claudecode_a_tail_with_no_conversation_is_read_far_back_once() {
        let p = std::env::temp_dir().join(format!("marspot-tail-bare-{}.jsonl", std::process::id()));
        let note = |n: usize| format!(r#"{{"type":"attachment","attachment":"{}"}}"#, "b".repeat(n));
        let said = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn"}}"#;
        let notes: String = (0..70).map(|_| note(32 * 1024) + "\n").collect();
        std::fs::write(&p, format!("{said}\n{notes}")).unwrap();
        let append = |s: &str| {
            let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
            std::io::Write::write_all(&mut f, s.as_bytes()).unwrap();
        };
        let mut tails = TailCache::default();
        let text = tails.tail(&p).unwrap();
        assert_eq!(activity_of(Some(&text), false), Activity::Unknown);
        assert!(text.len() as u64 <= TAIL, "only the usual window is kept: {}", text.len());
        assert!(tails.bytes >= super::MOST, "read back to the bound once: {}", tails.bytes);
        let before = tails.bytes;
        // half a record, then the rest of it
        append(&note(100)[..50]);
        tails.tail(&p).unwrap();
        append(&format!("{}\n", &note(100)[50..]));
        tails.tail(&p).unwrap();
        assert!(tails.bytes - before < 3 * TAIL, "the growth is read, not the bound again: {}", tails.bytes - before);
        assert_eq!(tails.reads, 3);
        append(&format!("{said}\n"));
        let text = tails.tail(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        assert_eq!(activity_of(Some(&text), false), Activity::AwaitingUser, "a record that came is found");
    }
}
