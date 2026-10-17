//! The CLI's own records of its live processes, `<config-dir>/sessions/<pid>.json`,
//! and the process identity everything that acts on a claude re-checks.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use marspot::pidtree::{self, ProcId};

use super::{json_string_field, looks_like_claudecode};

/// How far a process's start may trail its record's mtime and still be
/// the process that wrote it: the CLI writes the record moments after
/// it starts, and timestamps are not taken at the same instant.
const RECORD_SLACK: Duration = Duration::from_secs(2);

/// Whether `id` is still the process it was, and still claude by its
/// argv[0].  A pid read a scan ago can name another process now.
pub(super) fn still_claude(id: ProcId) -> bool {
    pidtree::proc_row(id.pid).is_some_and(|r| r.id() == id && looks_like_claudecode(&r))
}

/// The process a `sessions/<pid>.json` record speaks for, if it still
/// does: the pid is alive, it is claude, and it started no later than
/// the record was last written.  The directory keeps a file for every
/// pid that ever ran and pids are reused, so a live pid alone proves
/// nothing.
fn record_holder(record: &Path, pid: i32) -> Option<ProcId> {
    let written = fs::metadata(record).and_then(|m| m.modified()).ok()?;
    let row = pidtree::proc_row(pid)?;
    let started = SystemTime::UNIX_EPOCH + Duration::from_micros(row.start_us);
    (started <= written + RECORD_SLACK && looks_like_claudecode(&row)).then(|| row.id())
}

fn record_pid(path: &Path) -> Option<i32> {
    path.file_stem()?.to_str()?.parse().ok()
}

/// The claude session that would make a `--resume` of `uuid` fail, if
/// there is one.
///
/// Worth a stat before every cycle because of the order the cycle runs
/// in: the running claude is taken down *first*, and only then is the
/// resume line typed.  A resume that was never going to be accepted
/// therefore does not merely fail — the pane is left at a shell prompt
/// with the conversation no longer in it, and the way back is `claude
/// attach <id>` typed by hand.  The user saw a red paragraph and a
/// screenful of mouse reports instead (2026-09-01, uuid=1bcedee1).
///
/// The test mirrors the CLI's own, read out of `claude` 2.1.252: among
/// the live session records, a holder is one whose `sessionId` matches
/// and whose `kind` is anything other than `"interactive"` — which is
/// the case it reports as `Session <id> is running as a background
/// session`.
///
/// `config_dir` is the profile the *resuming* claude will run under,
/// because that is the directory it will read; whether that sees
/// another profile's sessions is the user's business (mine symlinks
/// them all to one shared dir, which is exactly why the refusal crossed
/// profiles at all).
///
/// Anything unreadable answers "no holder".  The gate is here to stop a
/// cycle already known to be futile, not to demand proof that one is
/// safe.
pub(super) fn background_session_holder(config_dir: &Path, uuid: &str) -> Option<BackgroundHolder> {
    if uuid.is_empty() {
        return None;
    }
    let needle = format!("\"sessionId\":\"{uuid}\"");
    for e in fs::read_dir(config_dir.join("sessions")).ok()?.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(pid) = record_pid(&path) else { continue };
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        if !body.contains(&needle) {
            continue;
        }
        // A record with no `kind` is not a holder — the CLI skips
        // those too rather than guessing what they are.
        let Some(kind) = json_string_field(&body, "\"kind\":\"") else {
            continue;
        };
        if kind == "interactive" {
            continue;
        }
        let Some(id) = record_holder(&path, pid) else { continue };
        let idle = json_string_field(&body, "\"status\":\"").as_deref() == Some("idle");
        return Some(BackgroundHolder { id, idle });
    }
    None
}

/// A live background claude holding a session a cycle wants to resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BackgroundHolder {
    pub(super) id: ProcId,
    /// The CLI's own `status` for it.  Idle means nothing is in flight:
    /// stopping it loses no work, only the process — the conversation is
    /// the transcript, and the cycle resumes that in the pane.
    pub(super) idle: bool,
}

/// What the CLI says one live claude process is doing, from
/// `<config-dir>/sessions/<pid>.json`.
pub(super) struct RecordedSession {
    pub(super) session_id: String,
    pub(super) kind: String,
}

/// The session a live claude is on, in its own words — `None` when the
/// process has no record (older CLI builds, or the record not written
/// yet), the record does not parse, or the record is not this live
/// process's (see [`record_holder`]).
pub(super) fn recorded_session(config_dir: &Path, pid: i32) -> Option<RecordedSession> {
    let path = config_dir.join("sessions").join(format!("{pid}.json"));
    record_holder(&path, pid)?;
    let body = fs::read_to_string(&path).ok()?;
    let session_id = json_string_field(&body, "\"sessionId\":\"")?;
    let kind = json_string_field(&body, "\"kind\":\"")?;
    (!session_id.is_empty()).then_some(RecordedSession { session_id, kind })
}

/// Every session a live non-interactive claude holds, per the records in
/// `config_dir` — the uuids no pane can be bound to.
pub(super) fn background_held_sessions(config_dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(config_dir.join("sessions")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let Some(pid) = record_pid(&e.path()) else { continue };
        if let Some(rec) = recorded_session(config_dir, pid)
            && rec.kind != "interactive"
        {
            out.push(rec.session_id);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Child, Command};

    /// A live process whose argv[0] is `claude`, killed on drop.
    struct FakeClaude(Child);
    impl FakeClaude {
        fn spawn() -> Self {
            Self(Command::new("/bin/sleep").arg0("claude").arg("60").spawn().unwrap())
        }
        fn pid(&self) -> i32 {
            self.0.id() as i32
        }
        fn id(&self) -> ProcId {
            pidtree::proc_id(self.pid()).unwrap()
        }
    }
    impl Drop for FakeClaude {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A `<config-dir>/sessions/` holding one record per `(pid, kind)`,
    /// all naming `uuid`.  Records are written the way claude writes
    /// them: one JSON object per file, named for the pid.
    fn tmp_config_with_sessions(uuid: &str, records: &[(i32, &str)]) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cc-bg-session-test-{}-{}", std::process::id(), n));
        let sessions = dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        for (pid, kind) in records {
            let body = format!(
                r#"{{"pid":{pid},"sessionId":"{uuid}","cwd":"/tmp","kind":"{kind}","status":"idle"}}"#
            );
            fs::write(sessions.join(format!("{pid}.json")), body).unwrap();
        }
        dir
    }

    /// The case the gate exists for: a live record for this uuid whose
    /// kind is not `interactive`.  `claude --resume` refuses this one,
    /// so the cycle must not take the running claude down for it.
    #[test]
    fn background_holder_found_for_live_non_interactive_record() {
        let uuid = "1bcedee1-2228-4e82-a9c2-632aac03bd2d";
        let c = FakeClaude::spawn();
        let dir = tmp_config_with_sessions(uuid, &[(c.pid(), "background")]);
        assert_eq!(background_session_holder(&dir, uuid), Some(BackgroundHolder { id: c.id(), idle: true }));
        fs::remove_dir_all(&dir).ok();
    }

    /// A busy background job is a holder too, but a different one: the
    /// cycle stops an idle job and refuses a busy one, so the status has
    /// to come through.
    #[test]
    fn a_busy_background_holder_says_so() {
        let uuid = "fb55e303-d796-4e20-80cf-d4be31c91a7a";
        let c = FakeClaude::spawn();
        let dir = tmp_config_with_sessions(uuid, &[(c.pid(), "bg")]);
        let f = dir.join("sessions").join(format!("{}.json", c.pid()));
        let body = fs::read_to_string(&f).unwrap().replace("\"idle\"", "\"busy\"");
        fs::write(&f, body).unwrap();
        assert_eq!(background_session_holder(&dir, uuid), Some(BackgroundHolder { id: c.id(), idle: false }));
        fs::remove_dir_all(&dir).ok();
    }

    /// The CLI's record for a pid is the session that process is on now,
    /// and it is what a pane binds to.  Only a live claude's record counts.
    #[test]
    fn a_live_claudes_record_names_its_session() {
        let uuid = "bcdc11f1-d813-4337-b30b-30a637e43238";
        let c = FakeClaude::spawn();
        let dir = tmp_config_with_sessions(uuid, &[(c.pid(), "interactive")]);
        let rec = recorded_session(&dir, c.pid()).expect("its record");
        assert_eq!(rec.session_id, uuid);
        assert_eq!(rec.kind, "interactive");
        // No file for this pid (and it is not alive either).
        assert!(recorded_session(&dir, i32::MAX).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    /// Sessions background jobs hold are excluded from the binding
    /// guesses; interactive ones are not.  2026-09-17: panes 390 and 391
    /// were bound to sessions two background jobs owned, and every
    /// profile switch was refused.
    #[test]
    fn only_background_jobs_sessions_are_held() {
        let c = FakeClaude::spawn();
        let held = "52e33087-2410-4f10-91f1-6d3182120ed6";
        let dir = tmp_config_with_sessions(held, &[(c.pid(), "bg")]);
        assert_eq!(background_held_sessions(&dir), vec![held.to_string()]);
        fs::remove_dir_all(&dir).ok();
        let dir = tmp_config_with_sessions("bcdc11f1-d813-4337-b30b-30a637e43238", &[(c.pid(), "interactive")]);
        assert!(background_held_sessions(&dir).is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    /// A record whose pid is alive but is not claude is a dead claude's
    /// file whose pid was reused -- here, by this test process.  It holds
    /// nothing, names no session, and so nothing would be signalled.
    #[test]
    fn a_record_whose_pid_is_not_claude_is_stale() {
        let uuid = "1bcedee1-2228-4e82-a9c2-632aac03bd2d";
        let me = std::process::id() as i32;
        let dir = tmp_config_with_sessions(uuid, &[(me, "background")]);
        assert_eq!(background_session_holder(&dir, uuid), None);
        assert!(recorded_session(&dir, me).is_none());
        assert!(background_held_sessions(&dir).is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    /// A claude that started after its record was last written did not
    /// write it: the pid was reused by a later claude.
    #[test]
    fn a_record_older_than_its_claude_is_stale() {
        let uuid = "dc928502-4c1c-4cdc-a1b8-ab993d342c64";
        let c = FakeClaude::spawn();
        let dir = tmp_config_with_sessions(uuid, &[(c.pid(), "background")]);
        let f = dir.join("sessions").join(format!("{}.json", c.pid()));
        let old = SystemTime::now() - Duration::from_secs(60);
        fs::File::options().write(true).open(&f).unwrap().set_modified(old).unwrap();
        assert_eq!(background_session_holder(&dir, uuid), None);
        assert!(recorded_session(&dir, c.pid()).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    /// What the acting paths check: the same process, and claude; not
    /// whatever holds the pid now.
    #[test]
    fn still_claude_wants_the_same_process() {
        let c = FakeClaude::spawn();
        let id = c.id();
        assert!(still_claude(id));
        assert!(!still_claude(ProcId { pid: id.pid, start_us: id.start_us + 1 }));
        assert!(!still_claude(pidtree::proc_id(std::process::id() as i32).unwrap()), "not claude");
    }

    /// The ordinary case — a pane running claude in the foreground has
    /// a record of its own, and cycling it is the whole feature.
    #[test]
    fn interactive_record_is_not_a_holder() {
        let uuid = "1bcedee1-2228-4e82-a9c2-632aac03bd2d";
        let c = FakeClaude::spawn();
        let dir = tmp_config_with_sessions(uuid, &[(c.pid(), "interactive")]);
        assert_eq!(background_session_holder(&dir, uuid), None);
        fs::remove_dir_all(&dir).ok();
    }

    /// The directory keeps a file for every pid that ever ran, so the
    /// common shape is a background record whose process is long gone.
    /// Blocking on those would make the cycle unusable after the first
    /// background job the project ever ran.
    #[test]
    fn dead_background_record_is_not_a_holder() {
        let uuid = "1bcedee1-2228-4e82-a9c2-632aac03bd2d";
        // 2^31-1 is never a live pid here.
        let dir = tmp_config_with_sessions(uuid, &[(i32::MAX, "background")]);
        assert_eq!(background_session_holder(&dir, uuid), None);
        fs::remove_dir_all(&dir).ok();
    }

    /// Another conversation's background job says nothing about this
    /// one — the match is on sessionId, not on "any background job in
    /// this profile".
    #[test]
    fn other_uuid_background_record_is_not_a_holder() {
        let mine = "1bcedee1-2228-4e82-a9c2-632aac03bd2d";
        let theirs = "dc928502-4c1c-4cdc-a1b8-ab993d342c64";
        let c = FakeClaude::spawn();
        let dir = tmp_config_with_sessions(theirs, &[(c.pid(), "background")]);
        assert_eq!(background_session_holder(&dir, mine), None);
        fs::remove_dir_all(&dir).ok();
    }

    /// A profile that has never run claude has no `sessions/` at all,
    /// and an unreadable directory must not stand in for "blocked" —
    /// the gate stops futile cycles, it does not gate on proof.
    #[test]
    fn missing_sessions_dir_is_not_a_holder() {
        let tmp = marspot_term::test_env::TempDir::new("cc-bg-none");
        let dir = tmp.join("nonexistent");
        assert_eq!(background_session_holder(&dir, "1bcedee1-2228-4e82-a9c2-632aac03bd2d"), None);
    }
}
