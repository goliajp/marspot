//! File-level access to agent histories: where they are, where the
//! last compaction is, and reading whole lines from an offset.
//!
//! Both agents append one JSON record per line and never rewrite, so a
//! byte offset is a stable bookmark.  Both files get big — a codex
//! rollout of 1.1 GB and a claude transcript of 317 MB are on this
//! machine — so nothing here reads a file whole: the last compaction is
//! found by scanning backwards, and reading forwards starts from it.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const BACK_CHUNK: u64 = 1 << 20;

/// The start of the last line at or after `floor` that contains
/// `needle`, or `None`.
///
/// `needle` must be something that cannot occur inside a JSON string —
/// a `"key":"value"` pair with its quotes, which inside a string would
/// be escaped as `\"`.  The caller still parses the line to confirm it.
pub fn last_line_with(path: &Path, needle: &[u8], floor: u64) -> std::io::Result<Option<u64>> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut end = len;
    let mut buf: Vec<u8> = Vec::new();
    while end > floor {
        let start = end.saturating_sub(BACK_CHUNK).max(floor);
        // Overlap by the needle's length so a match across the chunk
        // boundary is not missed.
        let read_end = (end + needle.len() as u64).min(len);
        buf.resize((read_end - start) as usize, 0);
        f.seek(SeekFrom::Start(start))?;
        f.read_exact(&mut buf)?;
        if let Some(i) = buf.windows(needle.len()).rposition(|w| w == needle) {
            return line_start_before(&mut f, start + i as u64, floor).map(Some);
        }
        end = start;
    }
    Ok(None)
}

/// Offset just after the last `\n` before `at` (or `floor`).
fn line_start_before(f: &mut File, at: u64, floor: u64) -> std::io::Result<u64> {
    let mut end = at;
    let mut buf: Vec<u8> = Vec::new();
    while end > floor {
        let start = end.saturating_sub(BACK_CHUNK).max(floor);
        buf.resize((end - start) as usize, 0);
        f.seek(SeekFrom::Start(start))?;
        f.read_exact(&mut buf)?;
        if let Some(i) = buf.iter().rposition(|&c| c == b'\n') {
            return Ok(start + i as u64 + 1);
        }
        end = start;
    }
    Ok(floor)
}

/// Call `each(offset, line)` for every COMPLETE line from `from`.
///
/// Returns the offset after the last complete line — where the next
/// read should start.  A last line with no newline yet is being written
/// by the agent right now; it is neither read nor counted, so it is
/// picked up whole next time instead of half now.
pub fn for_each_line(
    path: &Path,
    from: u64,
    mut each: impl FnMut(u64, &str),
) -> std::io::Result<u64> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    // A bookmark past the end means the file is not the one it was
    // taken on; start over rather than read nothing forever.
    let from = if from > len { 0 } else { from };
    f.seek(SeekFrom::Start(from))?;
    let mut r = BufReader::with_capacity(1 << 16, f);
    let mut at = from;
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        let n = r.read_until(b'\n', &mut line)?;
        if n == 0 || line.last() != Some(&b'\n') {
            return Ok(at);
        }
        let text = String::from_utf8_lossy(&line[..n - 1]);
        each(at, &text);
        at += n as u64;
    }
}

/// `<config>/projects/*/<uuid>.jsonl` — claude's transcript for a
/// session.  Profiles share `projects/` through a symlink here, but the
/// path is looked up under the config dir the session actually ran
/// with, so a profile that does not share still resolves.
pub fn claude_transcript(config_dir: &Path, uuid: &str) -> Option<PathBuf> {
    let name = format!("{uuid}.jsonl");
    std::fs::read_dir(config_dir.join("projects"))
        .ok()?
        .flatten()
        .map(|e| e.path().join(&name))
        .find(|p| p.is_file())
}

/// `<home>/sessions/YYYY/MM/DD/rollout-<ts>-<id>.jsonl` for a thread.
pub fn codex_rollout(codex_home: &Path, id: &str) -> Option<PathBuf> {
    let suffix = format!("-{id}.jsonl");
    let dirs = |p: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(p)
            .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
            .unwrap_or_default();
        v.sort();
        v.reverse(); // newest day first: the one wanted is usually recent
        v
    };
    for y in dirs(&codex_home.join("sessions")) {
        for m in dirs(&y) {
            for d in dirs(&m) {
                let Ok(rd) = std::fs::read_dir(&d) else { continue };
                for e in rd.flatten() {
                    let n = e.file_name();
                    if n.to_str().is_some_and(|n| n.starts_with("rollout-") && n.ends_with(&suffix)) {
                        return Some(e.path());
                    }
                }
            }
        }
    }
    None
}

/// The thread id in a rollout's file name.
pub fn codex_id_of(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    // rollout-2026-09-20T11-27-08-<36-char uuid>
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    (id.len() == 36 && id.chars().filter(|c| *c == '-').count() == 4).then(|| id.to_string())
}

#[cfg(test)]
pub fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marspot-handoff-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_match_is_found_from_the_end_across_chunks() {
        let d = tmp("scan");
        let p = d.join("h.jsonl");
        let filler = format!("{{\"type\":\"x\",\"pad\":\"{}\"}}\n", "y".repeat(1000));
        let mut s = String::new();
        s.push_str("{\"type\":\"compacted\",\"n\":1}\n");
        for _ in 0..3000 {
            s.push_str(&filler); // ~3 MB: several back-chunks
        }
        let second = s.len() as u64;
        let line = "{\"type\":\"compacted\",\"n\":2}\n";
        s.push_str(line);
        for _ in 0..3000 {
            s.push_str(&filler);
        }
        std::fs::write(&p, &s).unwrap();
        let got = last_line_with(&p, b"\"type\":\"compacted\"", 0).unwrap();
        assert_eq!(got, Some(second));
        assert_eq!(
            last_line_with(&p, b"\"type\":\"compacted\"", second + line.len() as u64).unwrap(),
            None,
            "nothing is found before the floor (a watermark is always a line start)"
        );
    }

    /// A half-written last line is left for next time.
    #[test]
    fn a_line_still_being_written_is_not_read() {
        let d = tmp("partial");
        let p = d.join("h.jsonl");
        std::fs::write(&p, "a\nbb\nccc").unwrap();
        let mut seen = Vec::new();
        let end = for_each_line(&p, 0, |at, l| seen.push((at, l.to_string()))).unwrap();
        assert_eq!(seen, vec![(0, "a".into()), (2, "bb".into())]);
        assert_eq!(end, 5);
        let end2 = for_each_line(&p, 99, |_, _| {}).unwrap();
        assert_eq!(end2, 5, "a bookmark past the end starts over");
    }

    #[test]
    fn files_are_found_by_id() {
        let d = tmp("find");
        let proj = d.join("projects/-w-a");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("u-1.jsonl"), "").unwrap();
        assert_eq!(claude_transcript(&d, "u-1"), Some(proj.join("u-1.jsonl")));
        assert_eq!(claude_transcript(&d, "u-2"), None);

        let id = "01a0bca3-87a2-7aa2-8d42-d84a71b8539a";
        let day = d.join("sessions/2026/09/20");
        std::fs::create_dir_all(&day).unwrap();
        let f = day.join(format!("rollout-2026-09-20T11-27-08-{id}.jsonl"));
        std::fs::write(&f, "").unwrap();
        assert_eq!(codex_rollout(&d, id), Some(f.clone()));
        assert_eq!(codex_id_of(&f).as_deref(), Some(id));
    }
}
