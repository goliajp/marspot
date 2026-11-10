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
    // Line 5 is the model exactly as claude names it, `[1m]` and all:
    // what a profile switch hands to `--model`, so the session comes
    // back on the model it was on and not on the new profile's
    // default. Readers older than this line stop at line 4.
    let id = payload
        .find("\"model\":{")
        .and_then(|at| json_string_field(&payload[at..], "\"id\":\""))
        .unwrap_or_default();
    if fs::write(
        &tmp,
        format!("{}\n{transcript}\n{effort}\npids={chain}\nid={id}\n", badge.model),
    )
    .is_ok()
    {
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
