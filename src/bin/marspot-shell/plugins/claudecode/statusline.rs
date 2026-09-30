//! The status-line hook: how claudecode is made to report the model
//! it is running, and how that report is read back.
//!
//! claude writes no model into its transcript, so the badge would have
//! nothing to say about a session until its first answer.  What it
//! does have is a status-line command it runs on every turn — so the
//! plugin installs itself there, chained ahead of whatever the user
//! already had, and each run drops one small record per session.
//!
//! Everything about editing someone else's settings file lives here
//! too: the chaining, the un-chaining, and the refusal to write a
//! settings file we cannot parse back.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use super::model::{ancestor_pids, short_effort, short_model, ModelBadge};

/// Where the status-line hook drops per-session model records: one
/// file per session, named by the session uuid — which is also the
/// transcript's file stem, so the badge side can look a record up
/// from the path it already computed.
pub(super) fn model_push_dir() -> PathBuf {
    marspot_term::paths::state_root()
        .join("plugins")
        .join("claudecode")
        .join("model")
}

/// How long a model record outlives its last write.
///
/// A session that ends simply stops re-writing its file, and nothing
/// else in the system knows the directory exists — so the writer
/// prunes, and every surviving session's next render clears out what
/// the dead ones left behind.  Three days is long enough that a
/// laptop closed over a weekend still finds its panes' records where
/// it left them.
pub(super) const MODEL_PUSH_TTL: Duration = Duration::from_secs(3 * 24 * 3600);

/// `marspot-shell --cc-statusline` — claudecode's status-line hook.
///
/// Every other route to "which model is this pane on" is an inference
/// from a lagging artefact.  The transcript names a model when an
/// assistant turn completes, or when `/model` prints its
/// confirmation, and says nothing in between: a switch made on a
/// parked pane, or a `--resume` under a different profile, left the
/// badge stating the *previous* model with full confidence until the
/// session next answered.  Tailing it faster cannot fix that — the
/// fact is not in the file yet.
///
/// Claude Code's status line is the one channel that carries what
/// claude itself currently believes.  It hands the command a JSON
/// payload containing `model.display_name` and re-runs it on state
/// change rather than on a timer (measured: one invocation per ~14 s
/// on an idle session, and one immediately at startup — which is what
/// closes the resume gap).
///
/// Prints nothing: claude renders empty status-line output as no line
/// at all, so installing this changes nothing on screen.  Always
/// exits 0 — a hook that fails is a warning inside the user's
/// session, and there is nothing here worth interrupting them for.
pub fn statusline_ingest() -> i32 {
    use std::io::Read;
    let mut payload = String::new();
    if std::io::stdin().read_to_string(&mut payload).is_err() {
        return 0;
    }
    let Some((sid, transcript, badge)) = statusline_fields(&payload) else {
        return 0;
    };
    let dir = model_push_dir();
    if fs::create_dir_all(&dir).is_err() {
        return 0;
    }
    // Rename so a badge reading mid-write sees the old record rather
    // than half of the new one.
    let tmp = dir.join(format!(".{sid}.tmp"));
    // Line 3 is the effort, blank when claude did not report one —
    // a record written before this field existed simply has two
    // lines, and reads back as "no effort said".
    let effort = badge.effort.clone().unwrap_or_default();
    // Line 4 names the claude that asked for this status line, as the
    // chain of pids from this hook up to it.
    //
    // The record is filed under the session uuid, which is the
    // transcript's stem — so the badge side can only find it once it
    // knows which transcript belongs to the pane.  For a session that
    // has taken no turn there is no transcript to know, and claude
    // writes one on the first turn: the record sits unread for as long
    // as the user has not sent anything.  Measured on this machine,
    // that window ran 8.8 hours on one pane, and the model half of the
    // badge was missing for all of it (2026-09-16).
    //
    // The pid chain is the key that does not need the transcript.  The
    // scanner already knows each pane's claude pid; this says which
    // claude ran the hook, so the two meet without the file.
    let chain = ancestor_pids(8)
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    if fs::write(&tmp, format!("{}\n{transcript}\n{effort}\npids={chain}\n", badge.model)).is_ok() {
        let _ = fs::rename(&tmp, dir.join(&sid));
    }
    prune_model_pushes(&dir);
    run_chained_statusline(&payload);
    0
}

/// Run the status line this hook replaced, if there was one.
///
/// Claude Code allows exactly one status-line command, so installing
/// over somebody's own line would silently take it away — and the
/// people most likely to want the model in the badge are the ones who
/// already care enough to have written a status line.  So the
/// installer does not take it: it moves the original into
/// `--chain <command>` and this runs it with the same payload,
/// relaying its output as if nothing were in between.
///
/// The command is carried in argv rather than in state of our own so
/// that the settings file stays the single description of what runs —
/// which is also what makes uninstalling it a matter of putting the
/// original string back.
pub(super) fn run_chained_statusline(payload: &str) {
    let mut args = std::env::args().skip(1);
    let Some(cmd) = args
        .find(|a| a == "--chain")
        .and_then(|_| args.next())
        .filter(|c| !c.is_empty())
    else {
        return;
    };
    use std::io::Write;
    let Ok(mut child) = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .stdin(std::process::Stdio::piped())
        .spawn()
    else {
        return;
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(payload.as_bytes());
    }
    let _ = child.wait();
}

/// Pull `(session uuid, transcript path, short model)` out of a
/// status-line payload.
///
/// `display_name` and not `id`: the display name is what the `/model`
/// menu shows, so the badge and the menu agree word for word, and the
/// id carries suffixes (`claude-opus-5[1m]`) that `short_model`
/// rejects outright.
pub(super) fn statusline_fields(payload: &str) -> Option<(String, String, ModelBadge)> {
    let sid = json_string_field(payload, "\"session_id\":\"")?;
    // The record's file name — reject anything that is not the uuid
    // shape rather than letting a payload name a path.
    if sid.is_empty()
        || !sid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    let transcript = json_string_field(payload, "\"transcript_path\":\"")?;
    let at = payload.find("\"model\":{")?;
    let name = json_string_field(&payload[at..], "\"display_name\":\"")?;
    let model = short_model(&name);
    if model.is_empty() {
        return None;
    }
    // Claude only sends `effort` for models that have one, so its
    // absence is an answer rather than a gap.
    let effort = payload
        .find("\"effort\":{")
        .and_then(|at| json_string_field(&payload[at..], "\"level\":\""))
        .and_then(|v| short_effort(&v));
    Some((sid, transcript, ModelBadge::new(model, effort)))
}

/// First string value for `key` (given with its quotes and colon).
/// The payload's strings are paths and display names — no embedded
/// quotes — so the first `"` ends the value.
pub(super) fn json_string_field(hay: &str, key: &str) -> Option<String> {
    let start = hay.find(key)? + key.len();
    let rest = &hay[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

pub(super) fn prune_model_pushes(dir: &std::path::Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for e in entries.flatten() {
        let stale = e
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > MODEL_PUSH_TTL);
        if stale {
            let _ = fs::remove_file(e.path());
        }
    }
}

// ── Registering the hook with Claude Code ─────────────────────────
//
// Claude Code learns about the hook from one line in its own
// `settings.json`.  There is no other channel: no environment
// variable, and the `--settings` flag covers a single launch, while
// most sessions are started by the user's own alias.
//
// Which makes this the one place marspot writes into another
// program's configuration, so the rules are strict:
//
//   * it happens only while `claudecode.statusline_hook` is on, which
//     is off by default and is a switch the user flips;
//   * a status line the user already wrote is never taken away — it
//     is chained (see `run_chained_statusline`) and put back on the
//     way out;
//   * turning the switch off restores the file, and the surrounding
//     text survives the round trip byte for byte;
//   * nothing is written that does not parse as JSON afterwards.
//
// Reconciliation is continuous rather than a one-off install step:
// the desired state is a setting, so the answer to "what if the user
// edits settings.json by hand" and "what if the binary moved" is the
// same answer, and neither needs a script that a shipped marspot
// would not have.

/// The shell binary version that first understood `--cc-statusline`.
///
/// Anything older falls through its CLI to the GUI start and comes up
/// as a full supervisor — with claude calling it on every render.
/// Checked rather than assumed.
pub(super) const MIN_HOOK_SHELL: (u32, u32, u32) = (0, 7, 116);

/// Every Claude Code settings file on this machine, deduplicated.
///
/// The profile directories commonly symlink one shared file;
/// canonicalising means it is read and written once rather than once
/// per profile.
pub(super) fn cc_settings_files() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let mut dirs = vec![home.join(".claude")];
    if let Ok(rd) = fs::read_dir(&home) {
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(".claude-profile-") {
                dirs.push(e.path());
            }
        }
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for d in dirs {
        let f = d.join("settings.json");
        let Ok(real) = f.canonicalize() else { continue };
        if !out.contains(&real) {
            out.push(real);
        }
    }
    out
}

/// The binary the hook should name: the newest one that knows the
/// flag, bundle first.
///
/// The bundle path is stable and a cold launch refreshes it; while an
/// older bundle is still pinned open by the running app,
/// `binaries/current/` holds the newer shell — which is the very
/// binary the bundle would exec into anyway.
pub(super) fn hook_binary() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(me) = std::env::current_exe()
        && let Some(dir) = me.parent() {
            cands.push(dir.join("marspot-shell"));
        }
    cands.push(
        marspot_term::paths::state_root()
            .join("binaries")
            .join("current")
            .join("marspot-shell"),
    );
    cands.into_iter().find(|c| shell_at_least(c, MIN_HOOK_SHELL))
}

pub(super) fn shell_at_least(bin: &std::path::Path, min: (u32, u32, u32)) -> bool {
    let Ok(out) = std::process::Command::new(bin)
        .arg("--version")
        .env("MARSPOT_NO_REDIRECT", "1")
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(v) = text
        .split_whitespace()
        .nth(1)
        .map(|v| v.trim_end_matches(|c: char| !c.is_ascii_digit()))
    else {
        return false;
    };
    let mut it = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let got = (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    );
    got >= min
}

/// Single-quote for `sh -c`, which is how claude runs the command.
///
/// Needed because the state root's path contains a space
/// ("Application Support"); unquoted, claude never invokes it at all.
pub(super) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub(super) fn hook_command(bin: &std::path::Path, chained: &str) -> String {
    let base = format!("{} --cc-statusline", sh_quote(&bin.to_string_lossy()));
    if chained.is_empty() {
        base
    } else {
        format!("{base} --chain {}", sh_quote(chained))
    }
}

/// The command our hook was told to run after itself, if any.
pub(super) fn chained_out_of(cmd: &str) -> String {
    let words = sh_split(cmd);
    match words.iter().position(|w| w == "--chain") {
        Some(i) => words.get(i + 1).cloned().unwrap_or_default(),
        None => String::new(),
    }
}

/// Enough of a shell word split to read back what `sh_quote` wrote.
pub(super) fn sh_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                any = true;
            }
            None if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            None => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The `statusLine` command in a settings file, if there is one.
///
/// A scan rather than a JSON parse: the file is the user's, it is
/// rewritten by cutting text so that their formatting survives, and
/// the same scan is what tells the cut where to start.
pub(super) fn status_line_command(text: &str) -> Option<(usize, String)> {
    let key = text.find("\"statusLine\"")?;
    let cmd_key = text[key..].find("\"command\"")? + key;
    let colon = text[cmd_key..].find(':')? + cmd_key;
    let open = text[colon..].find('"')? + colon + 1;
    let mut end = open;
    let bytes = text.as_bytes();
    while end < bytes.len() {
        match bytes[end] {
            b'\\' => end += 2,
            b'"' => break,
            _ => end += 1,
        }
    }
    let raw = text.get(open..end)?;
    Some((key, raw.replace("\\\"", "\"").replace("\\\\", "\\")))
}

pub(super) fn json_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Bring every Claude Code settings file in line with the switch.
///
/// Cheap when there is nothing to do — a canonicalize and a read of a
/// small file per config dir — and it does nothing at all when the
/// switch is off and no hook of ours is present, which is the state
/// every machine starts in.
///
/// Returns the lines worth logging; the caller decides where they go.
pub(super) fn reconcile_statusline_hook(want: bool) -> Vec<String> {
    // A sandbox shares the developer's HOME, so these are the
    // developer's own settings files — and the installed app is
    // reconciling them at the same time.  Two marspots arguing over one
    // config removed the hook the real one had just installed, once a
    // minute, for as long as a test ran (2026-09-28).  The sandbox
    // stays out.
    if std::env::var_os("MARSPOT_DEV_SANDBOX").is_some() {
        return vec![
            "sandbox: leaving the status-line hook alone (not this marspot's settings)".into(),
        ];
    }
    reconcile_statusline_in(want, hook_binary().as_deref(), &cc_settings_files())
}

/// The reconciliation itself, with the two things it reads off the
/// machine — which binary to name, and which files to edit — handed
/// in, so it can be exercised against a settings file that is not the
/// user's.
pub(super) fn reconcile_statusline_in(
    want: bool,
    bin: Option<&std::path::Path>,
    files: &[PathBuf],
) -> Vec<String> {
    let mut notes = Vec::new();
    for path in files {
        let path = path.as_path();
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let found = status_line_command(&text);
        let ours = found
            .as_ref()
            .is_some_and(|(_, c)| c.contains("--cc-statusline"));
        let new_text = match (want, &found, ours) {
            // Wanted and already ours: keep it naming a binary that
            // knows the flag.  The bundle's copy is replaced on a cold
            // launch, so this is how a hook installed against
            // `binaries/current/` moves back to the stable path.
            (true, Some((_, cmd)), true) => {
                let Some(bin) = bin else { continue };
                let want_cmd = hook_command(bin, &chained_out_of(cmd));
                if *cmd == want_cmd {
                    continue;
                }
                notes.push(format!("{}: repointed", path.display()));
                text.replacen(&json_quote(cmd), &json_quote(&want_cmd), 1)
            }
            // Wanted, and somebody else's status line is in the slot.
            // Claude Code allows exactly one, so take the slot and run
            // theirs from inside ours.
            (true, Some((_, cmd)), false) => {
                let Some(bin) = bin else { continue };
                notes.push(format!("{}: installed, chaining {cmd}", path.display()));
                text.replacen(&json_quote(cmd), &json_quote(&hook_command(bin, cmd)), 1)
            }
            // Wanted, nothing in the slot.
            (true, None, _) => {
                let Some(bin) = bin else {
                    notes.push(format!(
                        "{}: no marspot-shell new enough for the hook",
                        path.display()
                    ));
                    continue;
                };
                let Some(i) = text.find('{') else { continue };
                let block = format!(
                    "\n  \"statusLine\": {{ \"type\": \"command\", \"command\": {} }},",
                    json_quote(&hook_command(bin, ""))
                );
                notes.push(format!("{}: installed", path.display()));
                format!("{}{}{}", &text[..=i], block, &text[i + 1..])
            }
            // Not wanted, and ours is there: give the slot back.
            (false, Some((at, cmd)), true) => {
                let chained = chained_out_of(cmd);
                notes.push(format!("{}: removed", path.display()));
                if chained.is_empty() {
                    remove_status_line(&text, *at, cmd)
                } else {
                    text.replacen(&json_quote(cmd), &json_quote(&chained), 1)
                }
            }
            // Not wanted and not ours — nothing of ours to undo.
            (false, _, _) => continue,
        };
        // A settings.json that will not parse would lock the user out
        // of their own tool, so the edit has to prove itself first.
        if !json_parses(&new_text) {
            notes.push(format!("{}: edit refused — would not parse", path.display()));
            continue;
        }
        if let Err(e) = write_through_symlink(path, &new_text) {
            notes.push(format!("{}: {e}", path.display()));
        }
    }
    notes
}

/// `text` minus its `statusLine` member, the rest verbatim.
///
/// Cutting text rather than re-serialising: this is a file people
/// hand-edit, and a round trip through a JSON writer would reflow
/// every line of it to remove one key.
pub(super) fn remove_status_line(text: &str, at: usize, cmd: &str) -> String {
    let Some(rel) = text[at..].find('{') else {
        return text.to_string();
    };
    let mut j = at + rel;
    let b = text.as_bytes();
    let (mut depth, mut in_str, mut esc) = (0usize, false, false);
    while j < b.len() {
        let c = b[j];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        j += 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        j += 1;
    }
    debug_assert!(text[at..j].contains(cmd));
    // A member only comes out together with one of its commas.
    let mut start = at;
    let mut k = j;
    while b.get(k).is_some_and(|c| *c == b' ' || *c == b'\t') {
        k += 1;
    }
    if b.get(k) == Some(&b',') {
        k += 1;
    } else {
        // Last member — the comma joining it sits in front.
        let mut pre = start;
        while pre > 0 && b[pre - 1].is_ascii_whitespace() {
            pre -= 1;
        }
        if pre > 0 && b[pre - 1] == b',' {
            start = pre - 1;
        }
    }
    // Take the whole line when nothing else shares it, and one of the
    // two newlines bracketing it — whichever is there — so a file
    // written on a single line goes back to being one.
    let bol = text[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    if text[bol..start].trim().is_empty() {
        start = bol;
        if b.get(k) == Some(&b'\n') {
            k += 1;
        } else if start > 0 && b[start - 1] == b'\n' {
            start -= 1;
        }
    }
    format!("{}{}", &text[..start], &text[k..])
}

/// Structural check: braces, brackets and strings balance and the
/// text ends where they close.
///
/// Not a parser — the edits above only ever add or remove one whole
/// member, so what has to be caught is a stray comma or an unbalanced
/// brace, and that is what this catches.
pub(super) fn json_parses(text: &str) -> bool {
    let mut stack: Vec<u8> = Vec::new();
    let (mut in_str, mut esc, mut prev) = (false, false, 0u8);
    for &c in text.as_bytes() {
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' | b'[' => stack.push(c),
            b'}' | b']' => {
                let want = if c == b'}' { b'{' } else { b'[' };
                if stack.pop() != Some(want) || prev == b',' {
                    return false;
                }
            }
            b',' if prev == b',' => return false,
            _ => {}
        }
        if !c.is_ascii_whitespace() {
            prev = c;
        }
    }
    stack.is_empty() && !in_str
}

/// Replace the file's contents, keeping it the same file.
///
/// The profiles' `settings.json` are symlinks to one shared file;
/// renaming onto the link path would replace the link itself, so the
/// caller passes the canonical path and the temp file is made beside
/// it.
pub(super) fn write_through_symlink(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    let tmp = dir.join(format!(".marspot-settings-{}.tmp", std::process::id()));
    fs::write(&tmp, text)?;
    if let Ok(md) = fs::metadata(path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(md.permissions().mode()));
    }
    fs::rename(&tmp, path)
}
