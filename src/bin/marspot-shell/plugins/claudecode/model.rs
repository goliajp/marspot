//! What a pane's badge says about the model, and where that is read
//! from.
//!
//! Three sources, none of them optional: the session's own transcript
//! tail, the record the status-line hook pushed, and — before either
//! exists — claude's startup banner as drawn on the screen.

use std::path::PathBuf;

use super::*;

/// How much of the session jsonl tail to search for the active
/// model.  Single records (big tool_results) can run tens of KB, so
/// the window must comfortably span several of them.
pub(super) const MODEL_TAIL_BYTES: u64 = 262_144;

/// Tail the session jsonl and return the active model as a short
/// display token (e.g. `fable-5`).  Two producers, newest-in-file
/// wins:
///
///   - assistant records — authoritative, the model that actually
///     served the turn: `"role":"assistant"` + `"model":"claude-…"`
///   - `/model` slash-command output — `"subtype":"local_command"`
///     with `Set model to …` / `Kept model as …` in its content,
///     written the moment the user switches, so the badge follows a
///     `/model` change on the next 2 s tick instead of waiting for
///     the next assistant turn
///
/// Returns None when neither appears in the tail window (fresh
/// session, or a single giant record swamping the window) — the
/// badge then renders without the `@model` part.
pub(super) fn tail_model_short(path: &std::path::Path, min_offset: u64) -> Option<ModelBadge> {
    use std::cell::RefCell;
    // (mtime, size)-keyed memo so the 2 s scan tick only re-reads a
    // session's tail when the jsonl actually grew — idle panes cost
    // one `stat` per tick, not a 256 KB read.  Worker-thread-local;
    // capped so dead sessions can't accumulate entries forever.
    thread_local! {
        static CACHE: RefCell<
            HashMap<PathBuf, (SystemTime, u64, u64, Option<ModelBadge>)>,
        > = RefCell::new(HashMap::new());
    }
    const CACHE_CAP: usize = 64;
    let md = fs::metadata(path).ok()?;
    let mtime = md.modified().ok()?;
    let size = md.len();
    let hit = CACHE.with(|c| {
        c.borrow().get(path).and_then(|(t, s, off, v)| {
            (*t == mtime && *s == size && *off == min_offset).then(|| v.clone())
        })
    });
    if let Some(v) = hit {
        return v;
    }
    let result = tail_model_short_uncached(path, min_offset);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_CAP {
            c.clear();
        }
        c.insert(path.to_path_buf(), (mtime, size, min_offset, result.clone()));
    });
    result
}

pub(super) fn tail_model_short_uncached(path: &std::path::Path, min_offset: u64) -> Option<ModelBadge> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    // Never read behind the fence — those records describe a process
    // that no longer owns this session.
    let start = len.saturating_sub(MODEL_TAIL_BYTES).max(min_offset.min(len));
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = String::new();
    // Lossy is fine: we only pattern-scan ASCII keys, and a torn
    // first line simply won't match.
    let mut raw = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut raw).ok()?;
    buf.push_str(&String::from_utf8_lossy(&raw));
    for line in buf.lines().rev() {
        // /model output records vary by claudecode version: some
        // write `"type":"system","subtype":"local_command"`, some a
        // `"type":"user"` record whose content is the
        // `<local-command-stdout>` block.  Anchoring on the marker as
        // the DIRECT value of a content field (`"content":"<local-…`,
        // quotes unescaped) covers both — and is what makes the match
        // collision-proof against conversation text that merely
        // QUOTES these strings (a session where marspot itself is
        // being developed does exactly that): inside a text field the
        // quotes around `"content":` are JSON-escaped to `\"`, so the
        // unescaped anchor cannot occur.  The display name may carry
        // a trailing remark ("… and saved as your default for new
        // sessions"), so the cut also stops at " and ".
        for marker in [
            "\"content\":\"<local-command-stdout>Set model to ",
            "\"content\":\"<local-command-stdout>Kept model as ",
        ] {
            if let Some(i) = line.find(marker) {
                let rest = &line[i + marker.len()..];
                let mut end = rest.find(['<', '"']).unwrap_or(rest.len());
                if let Some(a) = rest.find(" and ") {
                    end = end.min(a);
                }
                let name = short_model(&rest[..end]);
                if !name.is_empty() {
                    // `/model` says nothing about effort.  Leaving it
                    // off is the honest reading: the model just
                    // changed, and what effort the new one runs at is
                    // something only the next turn will say.
                    return Some(ModelBadge::new(name, None));
                }
            }
        }
        if line.contains("\"role\":\"assistant\"")
            && let Some(i) = line.find("\"model\":\"") {
                let rest = &line[i + 9..];
                if let Some(end) = rest.find('"') {
                    let name = short_model(&rest[..end]);
                    if !name.is_empty() {
                        // The record carries the effort it ran at as
                        // a sibling of its own uuid, so the same line
                        // answers both halves.
                        let effort = line
                            .find("\"effort\":\"")
                            .map(|j| &line[j + 10..])
                            .and_then(|r| r.find('"').map(|e| &r[..e]))
                            .and_then(short_effort);
                        return Some(ModelBadge::new(name, effort));
                    }
                }
            }
    }
    None
}

/// Normalise a model identifier or display name into the short badge
/// token: strip ANSI escapes, the `claude-` prefix, a trailing
/// `-YYYYMMDD` snapshot date, and any parenthesised remark; lowercase
/// and map spaces / dots to `-` so `Opus 4.8` and `claude-opus-4-8`
/// both come out as `opus-4-8`.  Capped at 16 chars — the badge
/// shares the title strip with the session uuid.
pub(super) fn short_model(raw: &str) -> String {
    // jsonl strings carry control chars JSON-escaped — decode the
    // literal `\u001b` spelling into a real ESC before stripping.
    let raw = raw.replace("\\u001b", "\u{1b}");
    let raw = raw.as_str();
    // Strip ANSI CSI sequences (`ESC [ … letter`).
    let mut cleaned = String::with_capacity(raw.len());
    let mut it = raw.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for e in it.by_ref() {
                    if e.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        cleaned.push(c);
    }
    let cleaned = cleaned.trim();
    // Drop a parenthesised remark: "Default (recommended)" → "Default".
    let cleaned = match cleaned.find('(') {
        Some(i) => cleaned[..i].trim_end(),
        None => cleaned,
    };
    let cleaned = cleaned
        .strip_prefix("claude-")
        .unwrap_or(cleaned);
    // Trailing snapshot date: "-20251001".
    let cleaned = match cleaned.rfind('-') {
        Some(i)
            if cleaned.len() - i == 9
                && cleaned[i + 1..].bytes().all(|b| b.is_ascii_digit()) =>
        {
            &cleaned[..i]
        }
        _ => cleaned,
    };
    // Model identifiers and display names are ASCII words — any
    // other character means we grabbed prose, not a model; reject
    // the whole thing rather than render garbage in the badge.
    let mut out = String::with_capacity(cleaned.len());
    for c in cleaned.chars() {
        let mapped = match c {
            ' ' | '.' => '-',
            c if c.is_ascii_alphanumeric() || c == '-' => c.to_ascii_lowercase(),
            _ => return String::new(),
        };
        out.push(mapped);
        if out.len() >= 16 {
            break;
        }
    }
    out.trim_matches('-').to_string()
}

/// What the badge says about a pane's claude, past the profile tag.
///
/// The two travel together because every source that names one names
/// the other in the same breath — the status line's payload, the
/// assistant record, the startup banner's `Fable 5 with high effort`
/// — and splitting them would mean each source answering half a
/// question twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ModelBadge {
    pub(super) model: String,
    /// `None` where the source did not say: a model with no effort
    /// setting at all, or a claude too old to report one.  Rendered
    /// as absence rather than as a guess.
    pub(super) effort: Option<String>,
}

impl ModelBadge {
    pub(super) fn new(model: String, effort: Option<String>) -> Self {
        Self { model, effort }
    }

    /// `opus-5` / `opus-5·high`.
    ///
    /// The interpunct is claude's own separator on the banner line it
    /// reads this off (`Fable 5 with high effort · Claude Max`), so
    /// the badge and the screen it describes are punctuated alike.
    pub(super) fn render(&self) -> String {
        match &self.effort {
            Some(e) => format!("{}\u{b7}{e}", self.model),
            None => self.model.clone(),
        }
    }
}

/// Normalise an effort level (`high`, `xhigh`, `medium`, …).
///
/// Same shape as `short_model` and for the same reason: anything that
/// is not a plain ASCII word is prose we grabbed by accident, and the
/// badge is better off saying nothing than saying that.
pub(super) fn short_effort(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t.len() > 8 || !t.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some(t.to_ascii_lowercase())
}


/// The model claude last reported for this session through its
/// status-line hook, or None when the hook is not installed or has
/// not fired for this session yet.
pub(super) fn pushed_model(jsonl: &std::path::Path) -> Option<(ModelBadge, EffortSaid)> {
    let sid = jsonl.file_stem()?.to_str()?;
    let raw = fs::read_to_string(model_push_dir().join(sid)).ok()?;
    let mut lines = raw.lines();
    let model = lines.next()?.trim();
    if model.is_empty() {
        return None;
    }
    // Two lines is a record from 0.7.116-0.7.119, which had no third
    // line to write.  That is *unknown*, not *none* — and the
    // difference matters, because a parked pane can hold such a
    // record for days: claude only re-runs the hook when it redraws,
    // and a pane nobody is in never does.  Reported as unknown so the
    // effort half falls through to the transcript, which does know.
    let (said, effort) = match lines.nth(1) {
        Some(line) => (EffortSaid::Yes, short_effort(line)),
        None => (EffortSaid::No, None),
    };
    Some((ModelBadge::new(model.to_string(), effort), said))
}

/// The model and effort a session is running with, as claude names
/// them on its own command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct CliChoice {
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// What the status-line hook last recorded for session `uuid`, in the
/// form `--model` and `--effort` take.
///
/// A profile switch starts a new claude, and a new claude takes the
/// new profile's default unless told otherwise. On 2026-10-03 a pane
/// switched to Opus came back on Fable that way, on an account whose
/// Fable was spent. The record is a file on disk, so both values are
/// checked here rather than trusted.
pub(super) fn pushed_choice(uuid: &str) -> CliChoice {
    if uuid.is_empty() || !uuid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return CliChoice::default();
    }
    let Ok(raw) = fs::read_to_string(model_push_dir().join(uuid)) else {
        return CliChoice::default();
    };
    let lines: Vec<&str> = raw.lines().collect();
    let effort = lines
        .get(2)
        .and_then(|l| short_effort(l))
        .filter(|e| ["low", "medium", "high", "xhigh", "max"].contains(&e.as_str()));
    let model = lines
        .iter()
        .find_map(|l| l.strip_prefix("id="))
        .map(str::trim)
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 64
                && id.chars().all(|c| c.is_ascii_alphanumeric() || "-._[]".contains(c))
        })
        .map(str::to_string);
    CliChoice { model, effort }
}

/// This process's ancestors, nearest first, at most `max` of them.
///
/// Stops at the first pid it cannot read (pid 1, or a parent that
/// exited mid-walk) — a short chain is a smaller key, never a wrong
/// one.
pub(super) fn ancestor_pids(max: usize) -> Vec<i32> {
    let mut out = Vec::new();
    let mut pid = unsafe { libc::getppid() };
    while out.len() < max && pid > 1 {
        out.push(pid);
        match pidtree::proc_row(pid) {
            Some(row) => pid = row.ppid,
            None => break,
        }
    }
    out
}

/// What the status-line hook last said about the session running
/// under `claude_pid` — the route to the model (and the session uuid)
/// that does not wait for a transcript.
///
/// Matched on the pid chain the hook recorded, so two panes cwd'd
/// into the same project under the same profile cannot be confused
/// for one another; a record with no chain (written before 0.7.143)
/// is skipped rather than guessed at.
///
/// Ranked by how far up the chain the match sits, nearest first.  A
/// claude that spawns another claude — a subagent, or one started by
/// hand inside a pane — puts the outer one in the inner one's chain
/// too, and both records then name this pid; the nearer match is the
/// hook that ran closer to it.  Newest record breaks a tie: claude
/// re-runs the hook on every redraw, so the freshest is the one
/// describing what is on screen now.
/// The hook's record for the session named by `uuid`.
///
/// One file read.  `pushed_session_for_pid` walks the directory
/// because it is answering "which session is this process on"; here
/// the session is already known.
pub(super) fn pushed_by_uuid(uuid: &str) -> Option<ModelBadge> {
    if uuid.is_empty() {
        return None;
    }
    pushed_model(&PathBuf::from(format!("{uuid}.jsonl"))).map(|(m, _)| m)
}

pub(super) fn pushed_session_for_pid(claude_pid: i32) -> Option<(String, PathBuf, ModelBadge)> {
    // (distance up the chain, written at) — smaller distance wins,
    // newer breaks the tie.
    let mut best: Option<((usize, SystemTime), String, PathBuf, ModelBadge)> = None;
    for entry in fs::read_dir(model_push_dir()).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // `.uuid.tmp` — a record being written right now.
        if name.starts_with('.') {
            continue;
        }
        let Ok(raw) = fs::read_to_string(entry.path()) else { continue };
        let mut lines = raw.lines();
        let (Some(model), Some(transcript)) = (lines.next(), lines.next()) else {
            continue;
        };
        let effort = lines.next().and_then(short_effort);
        let Some(chain) = lines.next().and_then(|l| l.strip_prefix("pids=")) else {
            continue;
        };
        let Some(distance) = chain
            .split(',')
            .position(|p| p.parse::<i32>() == Ok(claude_pid))
        else {
            continue;
        };
        let model = model.trim();
        if model.is_empty() {
            continue;
        }
        let at = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let rank = (distance, at);
        if best
            .as_ref()
            .map(|(b, ..)| (rank.0, std::cmp::Reverse(rank.1)) < (b.0, std::cmp::Reverse(b.1)))
            .unwrap_or(true)
        {
            best = Some((
                rank,
                name,
                PathBuf::from(transcript),
                ModelBadge::new(model.to_string(), effort),
            ));
        }
    }
    best.map(|(_, uuid, transcript, badge)| (uuid, transcript, badge))
}

/// Whether a pushed record stated an effort at all — including
/// stating that there is none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EffortSaid {
    Yes,
    No,
}

///
/// The line sits directly under `Claude Code vX.Y.Z` and reads like
/// `Opus 5 (1M context) with high effort · Claude Max`.  Everything
/// after the model name is a remark — context window, effort, plan —
/// and `short_model` already drops parenthesised remarks and
/// lowercases, because it was written for `/model` output of the same
/// shape.  Cut at the first `·` so the plan name cannot leak in.
///
/// Anchored on the version line rather than on the model line's own
/// words: the model names change with every release, the frame around
/// them does not.
pub(super) fn parse_banner_model(screen: &str) -> Option<ModelBadge> {
    let mut lines = screen.lines();
    while let Some(line) = lines.next() {
        if !line.contains("Claude Code v") {
            continue;
        }
        // The next non-blank line is the model line.
        for next in lines.by_ref().take(3) {
            let t = next.trim();
            if t.is_empty() {
                continue;
            }
            if let Some(name) = banner_model_token(t) {
                return Some(name);
            }
            break;
        }
    }
    None
}

/// Pull the model out of a banner line, which is not the same thing
/// as cleaning the line up.
///
/// The line is decoration, name and qualifiers all at once:
///
/// ```text
///   ▛▀▜  Fable 5 with high effort · Claude Max
///        Opus 5 (1M context) with high effort · Claude Max
///        Sonnet 4.5 · Claude Pro
/// ```
///
/// Handing the whole thing to [`short_model`] used to fail two ways
/// at once, and the screenshot that prompted this had both.  The
/// ASCII-art logo shares these rows, and `short_model` rejects any
/// line containing a non-ASCII char — so the badge showed **no**
/// model.  And with the logo out of the way it produced
/// `fable-5-with-hig`: the effort suffix became part of the name and
/// then hit the 16-char cap.  The parenthesised form only ever
/// worked by accident — dropping everything from `(` happened to
/// drop ` with high effort` too, which is why `Opus 5 (1M context)`
/// looked fine while `Fable 5 with high effort` did not.
///
/// So this takes the name instead of trimming around it: skip
/// decoration, take the family word(s), take the version, and stop
/// at the first word that is neither.  Anything the banner adds
/// after the version — today `with high effort`, tomorrow something
/// else — ends the name rather than joining it.
pub(super) fn banner_model_token(line: &str) -> Option<ModelBadge> {
    let head = line.split('·').next().unwrap_or(line);
    // The name stops at a parenthesised remark; the effort qualifier
    // sits *after* it (`Opus 5 (1M context) with high effort`), so
    // the two are read off different spans of the same line.
    let name_span = match head.find('(') {
        Some(i) => &head[..i],
        None => head,
    };
    let mut name: Vec<&str> = Vec::new();
    let mut seen_version = false;
    for tok in name_span.split_whitespace() {
        // Strip decoration clinging to a word (`▟Fable`), then skip
        // tokens that are only decoration.
        let t = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if t.is_empty() {
            continue;
        }
        if !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
            // Mixed-script junk: before the name it is more
            // decoration, after it the name has ended.
            if name.is_empty() {
                continue;
            }
            break;
        }
        let is_version = t.chars().any(|c| c.is_ascii_digit());
        if is_version {
            seen_version = true;
            name.push(t);
            continue;
        }
        // Words are part of the name only until the version arrives —
        // "Claude Opus 5" is a name, "Fable 5 with" is a name and a
        // qualifier.
        if seen_version {
            break;
        }
        name.push(t);
    }
    if name.is_empty() {
        return None;
    }
    let joined = name.join(" ");
    let short = short_model(&joined);
    if short.is_empty() {
        return None;
    }
    // The qualifier the name loop stops at: `Fable 5 with high effort`.
    // It was being discarded as noise; it is the pane's effort level,
    // and it is the only place a just-resumed claude states it.
    let words: Vec<&str> = head.split_whitespace().collect();
    let effort = words
        .windows(3)
        .find(|w| w[0] == "with" && w[2].starts_with("effort"))
        .and_then(|w| short_effort(w[1]));
    Some(ModelBadge::new(short, effort))
}
