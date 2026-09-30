//! What model a codex pane is running, read off codex's own files.
//!
//! Two sources, in this order: the session's rollout (JSONL, appended
//! as the session runs) is the truth, and `config.toml` only answers
//! what a NEW session would start as — codex does not write the
//! model picker's choice back to it.

use std::path::{Path, PathBuf};

pub(super) fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
}

/// `model` and `model_reasoning_effort` from `~/.codex/config.toml`.
///
/// Read as text rather than parsed: the file is TOML with per-project
/// tables, and only two top-level scalars are wanted.  Stopping at the
/// first table header keeps a `[projects."…"]` section's own keys from
/// being mistaken for the globals.
pub(super) fn read_model_and_effort(home: &Path) -> (Option<String>, Option<String>) {
    let Ok(text) = std::fs::read_to_string(home.join("config.toml")) else {
        return (None, None);
    };
    let (mut model, mut effort) = (None, None);
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            break; // into per-project / per-feature tables
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let v = v.trim().trim_matches('"').to_string();
        match k.trim() {
            "model" => model = Some(v),
            "model_reasoning_effort" => effort = Some(v),
            _ => {}
        }
    }
    (model, effort)
}

/// What a codex session is ACTUALLY running, read from its own record.
///
/// The badge used to read `~/.codex/config.toml`, which says what a
/// FRESH codex would start with — not what the one in this pane is
/// doing.  Two panes on different efforts both showed the global
/// value, and a pane whose effort was changed mid-session showed the
/// old one.
///
/// Two records carry it.  `turn_context` is written per turn with
/// `cwd`, `model` and `effort` together; `thread_settings_applied` is
/// written the moment the user picks a model or effort, before any
/// turn runs, and carries the same three under `thread_settings`
/// (where the effort is spelled `reasoning_effort`).  The later of the
/// two is the truth: a pane whose model was just switched has no new
/// turn to describe it, and reading turns alone left the badge naming
/// the model the pane had before (2026-09-27 report — the screen said
/// astra, the corner said luna).  Matched to a pane by the codex
/// process's own working directory.
#[derive(Debug, PartialEq)]
pub(crate) struct SessionFacts {
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// The last line of `path` that names the model, found by walking the
/// file backwards.
///
/// The cheap tail is the normal source; this is for a session that
/// carries images or long tool output and can put megabytes between
/// two of those records.  The reported pane's own rollout had nothing
/// in its last 256 KiB, so it looked like a file belonging to no pane
/// and the badge was answered by a different session that shared the
/// working directory (2026-09-27).  Both record kinds are looked for
/// in one pass — a kind that is absent from the file costs a walk to
/// the floor, so asking separately pays that twice.
fn deep_fact_line(path: &std::path::Path) -> Option<String> {
    let len = std::fs::metadata(path).ok()?.len();
    let floor = len.saturating_sub(DEEP_SCAN_BYTES);
    let at = crate::plugins::handoff::history::last_line_with_any(
        path,
        &[br#""type":"turn_context""#, br#""thread_settings_applied""#],
        floor,
    )
    .ok()
    .flatten()?;
    let mut line = String::new();
    let _ = history_line_at(path, at, &mut line);
    (!line.is_empty()).then_some(line)
}

/// Read the single line starting at `at`.
fn history_line_at(path: &std::path::Path, at: u64, out: &mut String) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(at))?;
    let mut buf = Vec::new();
    BufReader::new(f.take(ROLLOUT_TAIL_BYTES)).read_until(b'\n', &mut buf)?;
    *out = String::from_utf8_lossy(&buf).trim_end().to_string();
    Ok(())
}

/// How far back a deep scan may look for a record.  A session that
/// goes this long without one has nothing to say about its model, and
/// the walk has to end somewhere.  Paid once per session: afterwards
/// the tail carries anything new.
const DEEP_SCAN_BYTES: u64 = 64 * 1024 * 1024;

/// The last line in `tail` that names the model.
fn last_fact_line(tail: &str) -> Option<&str> {
    tail.lines()
        .rev()
        .find(|l| l.contains("\"turn_context\"") || l.contains("\"thread_settings_applied\""))
}

/// Scan a rollout's TAIL for the last record that names the model.
///
/// These files reach tens of megabytes — 63 MB in the field — so
/// reading one whole, per pane, every two seconds is out of the
/// question.  The last record is near the end by construction, and a
/// window that misses it just leaves the caller with what it had.
pub(crate) fn facts_from_tail(tail: &str, want_cwd: &str) -> Option<SessionFacts> {
    tail.lines()
        .rev()
        .find_map(|line| facts_from_line(line, want_cwd))
}

/// The model and effort one record names, when it is this pane's.
///
/// Deliberately not a JSON parse: this is a hot-ish path, the fields
/// are flat strings, and a format change should degrade to "no facts"
/// rather than to a wrong badge.  The two kinds spell the effort
/// differently — `effort` in a turn, `reasoning_effort` in a settings
/// record.
fn facts_from_line(line: &str, want_cwd: &str) -> Option<SessionFacts> {
    let effort_key = if line.contains("\"turn_context\"") {
        "effort"
    } else if line.contains("\"thread_settings_applied\"") {
        "reasoning_effort"
    } else {
        return None;
    };
    if json_str_field(line, "cwd")? != want_cwd {
        return None;
    }
    Some(SessionFacts {
        model: json_str_field(line, "model"),
        effort: json_str_field(line, effort_key),
    })
}

/// The value of `"<key>":"<value>"`, first occurrence, no escapes.
fn json_str_field(line: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let at = line.find(&pat)? + pat.len();
    let rest = &line[at..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// How much of a rollout's end to look at for the last `turn_context`.
///
/// One record is a few hundred bytes and the last one is at the end by
/// construction; this is slack for whatever trails it.  Reading the
/// file whole is not an option — 63 MB in the field.
const ROLLOUT_TAIL_BYTES: u64 = 256 * 1024;

/// Rollouts under `<home>/sessions`, newest write first.
///
/// By modification time across every day-directory, not by the date in
/// the path.  A session is a file that gets appended to for as long as
/// it is open, so the one a pane is running now can have been created
/// weeks ago — the reported pane's session was ten days old, and a
/// window of the last two day-directories matched it to a DIFFERENT
/// session that shared its working directory (2026-09-27).
///
/// The cost is one `stat` per rollout, on the rescan cadence rather
/// than the tick: 535 files here, measured at a few milliseconds.  The
/// caller reads only the first handful of tails.
fn recent_rollouts(codex_home: &std::path::Path) -> Vec<PathBuf> {
    let mut out: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let sessions = codex_home.join("sessions");
    // sessions/YYYY/MM/DD/rollout-*.jsonl
    let mut days: Vec<PathBuf> = Vec::new();
    for y in read_dirs(&sessions) {
        for m in read_dirs(&y) {
            days.extend(read_dirs(&m));
        }
    }
    for day in &days {
        let Ok(rd) = std::fs::read_dir(day) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                out.push((m, p));
            }
        }
    }
    out.sort_by_key(|o| std::cmp::Reverse(o.0));
    out.into_iter().map(|(_, p)| p).collect()
}

fn read_dirs(at: &std::path::Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(at) else {
        return Vec::new();
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

/// Read the last `ROLLOUT_TAIL_BYTES` of a file as text.
fn tail_of(path: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let from = len.saturating_sub(ROLLOUT_TAIL_BYTES);
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::with_capacity(ROLLOUT_TAIL_BYTES as usize);
    f.take(ROLLOUT_TAIL_BYTES).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Which rollout belongs to which working directory, and what it last
/// said.
///
/// Finding a pane's rollout means opening files until one claims its
/// cwd, and there are dozens.  Doing that per pane per tick is the
/// wrong shape: the mapping barely changes, while the CONTENT of one
/// file changes constantly.  So the mapping is rebuilt on a slow
/// cadence and the content is re-read only when that file's mtime
/// moves — steady state is one `stat` per codex pane.
#[derive(Default)]
pub(crate) struct RolloutIndex {
    by_cwd: std::collections::HashMap<String, PathBuf>,
    facts: std::collections::HashMap<String, (std::time::SystemTime, SessionFacts)>,
    /// When a cwd was last searched for, so a pane whose session
    /// cannot be found does not pay for a search every tick.
    searched_at: std::collections::HashMap<String, std::time::Instant>,
    /// Files whose backward scan has been attempted.  Records are only
    /// ever appended, so a file with none in reach has none to find:
    /// anything written later lands in the tail, which is cheap.
    deep_done: std::collections::HashSet<PathBuf>,
    /// The scan that is running off this thread, and what it sends
    /// back.  A backward scan costs ~100 ms — the whole hook budget,
    /// and it was spent on the thread that draws the window, which
    /// logged a 110 ms overshoot the first time a pane was discovered
    /// after an image swap (2026-09-28).  It runs on a thread of its
    /// own now; the tick asks, and reads the answer on a later tick.
    deep: Option<(
        std::sync::mpsc::Sender<DeepScan>,
        std::sync::Mutex<std::sync::mpsc::Receiver<DeepScan>>,
    )>,
    /// One scan at a time: a discovery storm must not spawn a thread
    /// per session.
    deep_inflight: bool,
}

/// What a backward scan found, addressed to the cwd that asked.
struct DeepScan {
    cwd: String,
    mtime: std::time::SystemTime,
    facts: Option<SessionFacts>,
}

impl RolloutIndex {
    /// How often a cwd with no session yet is looked for again.  The
    /// search walks files; a pane that has one answers from the file
    /// it already knows and never gets here.
    const SEARCH_EVERY: std::time::Duration = std::time::Duration::from_secs(20);

    /// What the codex in `cwd` is running, or None when its record
    /// cannot be found — in which case the caller keeps what it had.
    ///
    /// Steady state is one file per pane: the session it is writing,
    /// re-read only when its mtime moves.  Searching for that file is
    /// what costs — it walks rollouts newest-first until one says it
    /// belongs to this working directory — so it happens once per
    /// pane, and at most every `SEARCH_EVERY` while that fails.  The
    /// shape this replaced re-read forty tails on a twenty-second
    /// cadence and took 190-340 ms on the thread that draws the
    /// window.
    /// Called once per tick, before any pane is asked about.
    ///
    /// Collects whatever the off-thread scans finished since the last
    /// one.  Cheap: a scan answers at most once, and most ticks have
    /// nothing waiting.
    pub(super) fn begin_tick(&mut self) {
        let Some((_, rx)) = self.deep.as_ref() else { return };
        let mut done: Vec<DeepScan> = Vec::new();
        {
            let rx = rx.lock().unwrap();
            while let Ok(d) = rx.try_recv() {
                done.push(d);
            }
        }
        for d in done {
            self.deep_inflight = false;
            if let Some(f) = d.facts {
                self.facts.insert(d.cwd, (d.mtime, f));
            }
        }
    }

    pub(super) fn facts_for_cwd(
        &mut self,
        codex_home: &std::path::Path,
        cwd: &str,
    ) -> Option<SessionFacts> {
        if let Some(path) = self.by_cwd.get(cwd).cloned() {
            if let Some(f) = self.facts_from_known(&path, cwd) {
                return Some(f);
            }
            // No facts does not mean the wrong file: the backward scan
            // may just be waiting for its turn.  The binding is what
            // the file's own head says, so ask that before looking
            // elsewhere — a session that ended, or one whose directory
            // changed, is the only reason to look again.
            if session_cwd(&path).as_deref() == Some(cwd) {
                return None;
            }
            self.by_cwd.remove(cwd);
        }
        let due = self
            .searched_at
            .get(cwd)
            .is_none_or(|t| t.elapsed() >= Self::SEARCH_EVERY);
        if !due {
            return None;
        }
        self.searched_at.insert(cwd.to_string(), std::time::Instant::now());
        let path = find_rollout_for_cwd(codex_home, cwd)?;
        // Bind first: the head named this cwd, which is the whole
        // reason this file is the pane's, and the facts may need a
        // later tick to be read.
        self.by_cwd.insert(cwd.to_string(), path.clone());
        self.facts_from_known(&path, cwd)
    }

    /// Facts from a file already known to be this cwd's.
    ///
    /// Cheap read first, and the deep walk at most once: new records
    /// are appended, so once this has looked deep it only ever needs
    /// the tail again.  What it knew stands until the tail says
    /// otherwise.
    fn facts_from_known(&mut self, path: &std::path::Path, cwd: &str) -> Option<SessionFacts> {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
        let known = self.facts.get(cwd).map(|(seen, f)| {
            (*seen, SessionFacts { model: f.model.clone(), effort: f.effort.clone() })
        });
        if let Some((seen, f)) = &known
            && *seen == mtime {
                return Some(SessionFacts { model: f.model.clone(), effort: f.effort.clone() });
            }
        if let Some(f) = tail_of(path)
            .as_deref()
            .and_then(last_fact_line)
            .and_then(|l| facts_from_line(l, cwd))
        {
            self.facts.insert(
                cwd.to_string(),
                (mtime, SessionFacts { model: f.model.clone(), effort: f.effort.clone() }),
            );
            return Some(f);
        }
        if let Some((_, f)) = known {
            self.facts.insert(
                cwd.to_string(),
                (mtime, SessionFacts { model: f.model.clone(), effort: f.effort.clone() }),
            );
            return Some(f);
        }
        if self.deep_inflight || !self.deep_done.insert(path.to_path_buf()) {
            return None;
        }
        let tx = self
            .deep
            .get_or_insert_with(|| {
                let (tx, rx) = std::sync::mpsc::channel();
                (tx, std::sync::Mutex::new(rx))
            })
            .0
            .clone();
        let (path, cwd) = (path.to_path_buf(), cwd.to_string());
        if std::thread::Builder::new()
            .name("codex-rollout-scan".into())
            .spawn(move || {
                let facts = deep_fact_line(&path).and_then(|l| facts_from_line(&l, &cwd));
                let _ = tx.send(DeepScan { cwd, mtime, facts });
            })
            .is_ok()
        {
            self.deep_inflight = true;
        }
        None
    }
}

/// The newest rollout opened in `cwd`.
///
/// By the `session_meta` record every rollout opens with, which names
/// the directory the session was started in.  The head, not the tail:
/// a long session can put megabytes between the records that name a
/// directory, so asking the end of the file is both dearer and less
/// certain — and matching the wrong file is how a pane came to wear
/// another session's model (2026-09-27).
///
/// Newest write first with an early exit: a live session is being
/// appended to, so it is at the front and the walk stops on the first
/// file that matches.
fn find_rollout_for_cwd(codex_home: &std::path::Path, cwd: &str) -> Option<PathBuf> {
    recent_rollouts(codex_home)
        .into_iter()
        .take(40)
        .find(|p| session_cwd(p).as_deref() == Some(cwd))
}

/// The directory a rollout was opened in, from its first record.
fn session_cwd(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let mut buf = vec![0u8; SESSION_META_BYTES];
    let mut f = std::fs::File::open(path).ok()?;
    let n = f.read(&mut buf).ok()?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let line = head.lines().next()?;
    line.contains("\"session_meta\"")
        .then(|| json_str_field(line, "cwd"))
        .flatten()
}

/// How much of a rollout's first line to read.  It carries the whole
/// system prompt, so it is tens of kilobytes; the fields wanted here
/// are at its front.
const SESSION_META_BYTES: usize = 64 * 1024;


/// The newest rollout opened in `cwd` — the same rule codex's own
/// `resume --last` uses.
pub(crate) fn newest_rollout_for_cwd(codex_home: &std::path::Path, cwd: &str) -> Option<PathBuf> {
    find_rollout_for_cwd(codex_home, cwd)
}

#[cfg(test)]
mod session_facts_tests {
    use super::{facts_from_tail, SessionFacts};

    /// Shape taken from a real rollout: `turn_context` carries cwd,
    /// model and effort in one record, which is why the last one is
    /// the whole answer.
    fn turn(cwd: &str, model: &str, effort: &str) -> String {
        format!(
            r#"{{"type":"turn_context","payload":{{"turn_id":"x","cwd":"{cwd}","model":"{model}","effort":"{effort}","summary":"auto"}}}}"#
        )
    }

    /// The shape codex writes the moment a model is picked, trimmed
    /// to the fields the badge reads.  Note `reasoning_effort` — the
    /// turn record spells the same thing `effort`.
    fn settings(cwd: &str, model: &str, effort: &str) -> String {
        format!(
            r#"{{"type":"event_msg","payload":{{"type":"thread_settings_applied","thread_id":"t","thread_settings":{{"model":"{model}","model_provider_id":"openai","cwd":"{cwd}","reasoning_effort":"{effort}"}}}}}}"#
        )
    }

    /// Picking a model takes effect before the next turn.
    ///
    /// Reported 2026-09-27: the picker said astra, the corner said
    /// luna.  The session had not taken a turn since the switch, so
    /// the newest turn record still named the old model — and with no
    /// turn record in the window at all, the badge fell through to a
    /// config file and named something the pane had never run.
    #[test]
    fn a_model_picked_since_the_last_turn_is_the_one_shown() {
        let tail = [
            turn("/w/a", "gpt-6-luna", "medium"),
            settings("/w/a", "gpt-6-astra", "medium"),
        ]
        .join("\n");
        assert_eq!(
            facts_from_tail(&tail, "/w/a"),
            Some(SessionFacts {
                model: Some("gpt-6-astra".into()),
                effort: Some("medium".into())
            })
        );
        // And a turn that comes after the switch is newer still.
        let tail = [tail, turn("/w/a", "gpt-6-astra", "high")].join("\n");
        assert_eq!(
            facts_from_tail(&tail, "/w/a"),
            Some(SessionFacts {
                model: Some("gpt-6-astra".into()),
                effort: Some("high".into())
            })
        );
    }

    /// A record further back than the cheap tail is still found.
    ///
    /// The reported session put megabytes of tool output between two
    /// of them, so its last 256 KiB named nothing and the file looked
    /// like it belonged to no pane at all (2026-09-27).
    #[test]
    fn a_record_beyond_the_tail_window_is_still_read() {
        let dir = std::env::temp_dir().join(format!("codex-deep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-2026-09-17T03-13-37-x.jsonl");
        let filler = format!("{{\"type\":\"response_item\",\"pad\":\"{}\"}}\n", "y".repeat(4000));
        let mut body = settings("/w/a", "gpt-6-astra", "medium");
        body.push('\n');
        for _ in 0..100 {
            body.push_str(&filler); // ~400 KiB past the record
        }
        std::fs::write(&path, &body).unwrap();

        assert!(
            super::last_fact_line(&super::tail_of(&path).unwrap()).is_none(),
            "the cheap tail must not see it, or this test proves nothing"
        );
        let line = super::deep_fact_line(&path).expect("the deep scan finds it");
        assert_eq!(
            super::facts_from_line(&line, "/w/a"),
            Some(SessionFacts {
                model: Some("gpt-6-astra".into()),
                effort: Some("medium".into())
            })
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The backward scan runs off the tick, and once per file ever.
    ///
    /// A scan of a file whose records sit past the tail window costs
    /// about as much as the whole hook budget (measured 68-103 ms, and
    /// 100 ms and 110 ms overshoots were logged the two times it
    /// shipped on the tick's own thread), so the tick asks for it and
    /// reads the answer on a later tick.  A file is asked about at
    /// most once either way: records are only appended, so whatever is
    /// written after a fruitless scan arrives in the tail, where
    /// reading it is cheap.
    #[test]
    fn a_backward_scan_runs_off_the_tick_and_once_per_file() {
        let home = std::env::temp_dir().join(format!("codex-budget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let day = home.join("sessions/2026/09/17");
        std::fs::create_dir_all(&day).unwrap();
        let filler = format!("{{\"type\":\"response_item\",\"pad\":\"{}\"}}\n", "y".repeat(4000));
        let write = |name: &str, cwd: &str| {
            let mut body = format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{cwd}\"}}}}\n"
            );
            body.push_str(&settings(cwd, "gpt-6-astra", "medium"));
            body.push('\n');
            for _ in 0..100 {
                body.push_str(&filler); // ~400 KiB past the record
            }
            std::fs::write(day.join(name), body).unwrap();
        };
        write("rollout-a.jsonl", "/w/a");

        let mut idx = super::RolloutIndex::default();
        idx.begin_tick();
        assert_eq!(
            idx.facts_for_cwd(&home, "/w/a"),
            None,
            "the tick that asks does not wait for the answer",
        );
        // The scan is a thread; give it its turn, the way later ticks do.
        let mut facts = None;
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            idx.begin_tick();
            facts = idx.facts_for_cwd(&home, "/w/a");
            if facts.is_some() {
                break;
            }
        }
        assert_eq!(
            facts,
            Some(SessionFacts {
                model: Some("gpt-6-astra".into()),
                effort: Some("medium".into())
            }),
            "a later tick reads what the scan found",
        );

        // Nothing to find, and looking again is the expensive mistake.
        std::fs::write(day.join("rollout-c.jsonl"), format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"/w/c\"}}}}\n{filler}"
        )).unwrap();
        idx.begin_tick();
        assert_eq!(idx.facts_for_cwd(&home, "/w/c"), None, "no record to read");
        assert!(
            idx.deep_done.contains(&day.join("rollout-c.jsonl")),
            "and it is not asked about twice",
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A settings record belongs to its own pane, like a turn does.
    #[test]
    fn a_settings_record_from_another_pane_does_not_answer() {
        let tail = settings("/w/other", "gpt-6-astra", "high");
        assert_eq!(facts_from_tail(&tail, "/w/mine"), None);
    }

    #[test]
    fn the_last_turn_wins() {
        let tail = [
            turn("/w/a", "gpt-6-astra", "low"),
            r#"{"type":"response_item","payload":{}}"#.to_string(),
            turn("/w/a", "gpt-6-astra", "high"),
        ]
        .join("\n");
        assert_eq!(
            facts_from_tail(&tail, "/w/a"),
            Some(SessionFacts {
                model: Some("gpt-6-astra".into()),
                effort: Some("high".into())
            }),
            "a mid-session change is the point of reading this at all"
        );
    }

    /// Another pane's session must not answer for this one.  The
    /// rollouts all live in one directory; cwd is what tells them
    /// apart.
    #[test]
    fn a_different_cwd_is_a_different_pane() {
        let tail = turn("/w/other", "gpt-6-astra", "high");
        assert_eq!(facts_from_tail(&tail, "/w/mine"), None);
    }

    /// A window that missed the record, or a format that changed,
    /// leaves the caller with what it had — never with a wrong badge.
    #[test]
    fn nothing_recognisable_yields_nothing() {
        assert_eq!(facts_from_tail("", "/w/a"), None);
        assert_eq!(facts_from_tail("not json at all\nnor this", "/w/a"), None);
        assert_eq!(
            facts_from_tail(r#"{"type":"turn_context","payload":{"cwd":"/w/a"}}"#, "/w/a"),
            Some(SessionFacts { model: None, effort: None }),
            "a record without the fields is still that pane's record"
        );
    }

    /// The tail can begin mid-line; a half record must not be read as
    /// a whole one.
    #[test]
    fn a_truncated_leading_line_is_skipped() {
        let whole = turn("/w/a", "gpt-6-astra", "high");
        let tail = format!("ext\":\"payload\":{{\"cwd\":\"/w/a\",\"model\":\"WRONG\"\n{whole}");
        let got = facts_from_tail(&tail, "/w/a").expect("the whole record is there");
        assert_eq!(got.model.as_deref(), Some("gpt-6-astra"));
    }
}

#[cfg(test)]
mod config_tests {
    use super::read_model_and_effort;

    /// Only the top-level scalars count.  `~/.codex/config.toml` carries
    /// `[projects."…"]` tables whose keys would otherwise be read as the
    /// global model, and the badge would follow whichever project was
    /// listed last rather than what codex is actually running.
    #[test]
    fn per_project_tables_do_not_leak_into_the_globals() {
        let dir = std::env::temp_dir().join(format!("codexcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "model = \"gpt-6-astra\"\nmodel_reasoning_effort = \"high\"\n\
             [projects.\"/x\"]\nmodel = \"WRONG\"\n",
        )
        .unwrap();
        let got = read_model_and_effort(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(got, (Some("gpt-6-astra".into()), Some("high".into())));
    }
}
