//! Which session a pane is on, when claude's own word is not enough.
//!
//! The worker's scan binds panes in passes, proof first and guesses
//! after; these are the guesses, and the check on the proof.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use super::SessionInfo;
use super::worker::WorkerCtx;

impl WorkerCtx {
    /// Reverse-lookup: encoded project dir → newest known session
    /// (id + its jsonl path, so callers can tail per-session state
    /// like the active model).  Cheap scan over `seen`; a dozen
    /// projects active in practice.
    ///
    /// Two constraints make this a *guess with guardrails* rather than
    /// a free-for-all:
    ///
    /// * `claimed` — a uuid already bound to another pane is skipped.
    ///   Two panes cwd'd into one project used to receive the identical
    ///   badge, which is provably wrong for at least one of them
    ///   (2026-07-30: sessions 383 + 394 both in `qualcomm/insight`,
    ///   both badged `9e304c9a`, which argv shows belongs to 383).
    /// * `claude_start` — a session whose file has not been written
    ///   since this claude process started cannot be the session it is
    ///   writing.  Without this, a pane freshly `claude`d in a project
    ///   whose newest session is days old wears that dead session's
    ///   uuid.  No eligible candidate ⇒ no badge, which is the honest
    ///   answer until the pane's own session file appears.
    pub(super) fn session_for_project(
        &self,
        encoded_dir: &str,
        claimed: &std::collections::HashSet<String>,
        claude_start: SystemTime,
    ) -> Option<(String, PathBuf)> {
        let mut newest: Option<(SystemTime, &SessionInfo)> = None;
        for s in self.seen.values() {
            if s.project_dir != encoded_dir {
                continue;
            }
            if claimed.contains(&s.session_id) {
                continue;
            }
            if s.last_mtime < claude_start {
                continue;
            }
            // nothing to resume and nothing being worked in
            if !s.has_conversation {
                continue;
            }
            match newest {
                Some((t, _)) if t >= s.last_mtime => {}
                _ => newest = Some((s.last_mtime, s)),
            }
        }
        newest.map(|(_, s)| (s.session_id.clone(), s.jsonl_path.clone()))
    }

    /// The session that has superseded `argv_uuid` in this project, if
    /// one has.
    ///
    /// **Why argv is not enough.** `claude --resume X` puts X in argv
    /// and leaves it there for the life of the process.  A `/clear`
    /// starts a different session in the same process; so does an
    /// in-session `/resume`.  Bind by argv alone and the pane keeps
    /// naming a transcript nobody is writing any more — the badge
    /// tails a dead file for its model, reclamation parks and resumes
    /// the wrong conversation, and switching profile brings back a
    /// session the user left hours ago (2026-08-11 report: `torajs`,
    /// argv `--resume f7a8a54b` while the live transcript was
    /// `e024458b`, 3 minutes newer and still growing).
    ///
    /// The only evidence available is which transcript is being
    /// appended to — claude closes the file between writes, so there
    /// is no descriptor to inspect.  So: the newest unclaimed session
    /// of this project, provided it has been written **since this
    /// claude started** (older ones belong to other runs) and is
    /// clearly newer than the argv one.
    ///
    /// `SUPERSEDE_MARGIN` keeps a pane from flip-flopping between two
    /// files touched in the same instant at startup; a real `/clear`
    /// leaves the old transcript untouched from then on, so the margin
    /// costs nothing there.
    ///
    /// Known limit: two panes on **one** project cannot be told apart
    /// this way — both see the same newest file.  The claim set hands
    /// it to the lower `shelld_sid` and the other keeps its argv, which
    /// is the same tie-break the guess path has always used.
    pub(super) fn successor_session(
        &self,
        encoded_dir: &str,
        claimed: &std::collections::HashSet<String>,
        claude_start: SystemTime,
        argv_uuid: &str,
    ) -> Option<String> {
        const SUPERSEDE_MARGIN: Duration = Duration::from_secs(5);
        let argv_mtime = self.session_by_uuid(argv_uuid).map(|s| s.last_mtime);
        let mut best: Option<(SystemTime, &SessionInfo)> = None;
        for s in self.seen.values() {
            if s.project_dir != encoded_dir
                || s.session_id == argv_uuid
                || claimed.contains(&s.session_id)
                || s.last_mtime < claude_start
                || !s.has_conversation
            {
                continue;
            }
            if let Some(t) = argv_mtime
                && s.last_mtime < t + SUPERSEDE_MARGIN {
                    continue;
                }
            match best {
                Some((t, _)) if t >= s.last_mtime => {}
                _ => best = Some((s.last_mtime, s)),
            }
        }
        best.map(|(_, s)| s.session_id.clone())
    }

    /// The session to bind instead of the one claude's own record
    /// names, when that one is a transcript without a conversation.
    ///
    /// The record is the best evidence there is and normally wins
    /// outright. It was seen naming a 200-byte file holding only a
    /// title (`ai-title` + `agent-name`) while the same process went
    /// on writing its real session; the profile
    /// switch that followed resumed the title file, claude answered
    /// "No conversation found", and the pane was left at a shell.
    ///
    /// Only a record whose file exists and is empty of conversation is
    /// second-guessed. A record with no file at all is a session that
    /// has not taken its first turn (after `/clear`, say), and the
    /// transcript written before it is exactly what must not be picked.
    pub(super) fn successor_of_empty_record(
        &self,
        recorded: &str,
        encoded_dir: &str,
        claimed: &std::collections::HashSet<String>,
        claude_start: SystemTime,
    ) -> Option<String> {
        let rec = self.session_by_uuid(recorded)?;
        if rec.has_conversation {
            return None;
        }
        self.successor_session(encoded_dir, claimed, claude_start, recorded)
    }

    /// Look a session up by uuid — the argv-authoritative path knows
    /// *which* session a pane owns but still needs its jsonl to tail
    /// the active model.  `None` for a session too new to have been
    /// scanned yet; the badge then carries no model until it is.
    pub(super) fn session_by_uuid(&self, uuid: &str) -> Option<&SessionInfo> {
        self.seen.values().find(|s| s.session_id == uuid)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    use super::*;
    use crate::plugins::claudecode::ShelldClient;

    fn session(uuid: &str, mtime: SystemTime, has_conversation: bool) -> SessionInfo {
        SessionInfo {
            session_id: uuid.to_string(),
            project_dir: "-p-lab".into(),
            jsonl_path: PathBuf::from(format!("/fake/-p-lab/{uuid}.jsonl")),
            last_mtime: mtime,
            last_size: 1,
            has_conversation,
        }
    }

    fn ctx(sessions: Vec<SessionInfo>) -> WorkerCtx {
        WorkerCtx {
            projects_root: PathBuf::from("/fake"),
            shelld: Arc::new(ShelldClient::new(None)),
            statusline_state: None,
            seen: sessions.into_iter().map(|s| (s.jsonl_path.clone(), s)).collect(),
            model_cutoff: HashMap::new(),
            banner_tried: HashMap::new(),
            last_refusal: HashMap::new(),
            last_model: HashMap::new(),
            last_model_by_pane: HashMap::new(),
        }
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + secs)
    }

    #[test]
    fn a_record_naming_a_title_only_file_gives_way_to_the_live_session() {
        // the process started at 0, its record points at a title file
        // written at 100, and its real transcript is still growing at 1600
        let c = ctx(vec![session("title", at(100), false), session("live", at(1600), true)]);
        assert_eq!(
            c.successor_of_empty_record("title", "-p-lab", &HashSet::new(), at(0)).as_deref(),
            Some("live"),
        );
    }

    #[test]
    fn a_record_naming_a_real_conversation_is_kept() {
        let c = ctx(vec![session("rec", at(100), true), session("other", at(1600), true)]);
        assert_eq!(
            c.successor_of_empty_record("rec", "-p-lab", &HashSet::new(), at(0)),
            None,
        );
    }

    #[test]
    fn a_record_with_no_file_yet_is_kept() {
        // after /clear the new session has no transcript until its first
        // turn, and the one written before it is the session just left
        let c = ctx(vec![session("before-clear", at(1600), true)]);
        assert_eq!(
            c.successor_of_empty_record("after-clear", "-p-lab", &HashSet::new(), at(0)),
            None,
        );
    }

    #[test]
    fn a_title_file_written_after_the_conversation_stopped_is_not_replaced_by_it() {
        // the conversation predates the title file: nothing newer to move to
        let c = ctx(vec![session("old", at(50), true), session("title", at(100), false)]);
        assert_eq!(
            c.successor_of_empty_record("title", "-p-lab", &HashSet::new(), at(0)),
            None,
        );
    }

    #[test]
    fn guesses_never_land_on_a_title_only_file() {
        let c = ctx(vec![session("title", at(1600), false), session("live", at(900), true)]);
        let none = HashSet::new();
        assert_eq!(
            c.session_for_project("-p-lab", &none, at(0)).map(|(u, _)| u).as_deref(),
            Some("live"),
            "the newest file is the title file, and it is not a session anyone is in"
        );
        assert_eq!(
            c.successor_session("-p-lab", &none, at(0), "argv").as_deref(),
            Some("live"),
        );
    }
}
