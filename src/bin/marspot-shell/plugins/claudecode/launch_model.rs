//! The model marspot names on every claude it starts.
//!
//! Every account's `settings.json` is a link to one shared file, so a
//! `/model` typed in any session changes what a claude started without
//! `--model` runs on, for every account at once. Nothing marspot starts
//! relies on that file: each launch says `--model`, and the value is
//! read from what was observed, nearest first:
//!
//! 1. what the caller asked for, taken as it is;
//! 2. the session's own record: the status-line hook's `id=` line, then
//!    the newest assistant record of its transcript;
//! 3. the pane the launch acts for (a helper's parent): that pane's
//!    claude, read the same way;
//! 4. the account's last observed model: the newest hook record whose
//!    transcript lives under the account's config directory;
//! 5. [`DEFAULT_MODEL`], said in a log line by the caller.

use std::path::Path;
use std::time::SystemTime;

use super::model::{cli_model_ok, pushed_choice, pushed_session_for_pid};
use super::statusline::model_push_dir;

/// The model a claude starts on when nothing about it was ever
/// observed. claude's own alias for its current Opus.
pub(in crate::plugins) const DEFAULT_MODEL: &str = "opus";

/// Where a launch's model came from, as the log says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::plugins) enum Source {
    Session,
    Pane,
    Account,
    Default,
}

impl Source {
    pub(in crate::plugins) fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Pane => "pane",
            Self::Account => "account",
            Self::Default => "default",
        }
    }
}

/// The first source that names a model claude takes; the default when
/// none does.
pub(in crate::plugins) fn first_known(sources: &[(Source, &dyn Fn() -> Option<String>)]) -> (String, Source) {
    sources
        .iter()
        .find_map(|(s, read)| read().filter(|m| cli_model_ok(m)).map(|m| (m, *s)))
        .unwrap_or_else(|| (DEFAULT_MODEL.to_string(), Source::Default))
}

/// The model session `uuid` runs on: the hook's record, else the newest
/// assistant record of `transcript`.
pub(in crate::plugins) fn session_model(uuid: &str, transcript: Option<&Path>) -> Option<String> {
    pushed_choice(uuid).model.or_else(|| transcript.and_then(transcript_model))
}

/// The model id of the newest assistant record in the transcript's
/// tail, as claude wrote it (`claude-opus-5-5`); an error record's
/// `<synthetic>` is not a model and is passed over.
pub(in crate::plugins) fn transcript_model(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(super::model::MODEL_TAIL_BYTES))).ok()?;
    let mut raw = Vec::new();
    f.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw);
    text.lines().rev().filter(|l| l.contains("\"role\":\"assistant\"")).find_map(|l| {
        let rest = &l[l.find("\"model\":\"")? + 9..];
        let id = &rest[..rest.find('"')?];
        cli_model_ok(id).then(|| id.to_string())
    })
}

/// The model the claude in pane `sid` runs on, by its hook record.
pub(in crate::plugins) fn pane_model(sid: u64) -> Option<String> {
    let obs = crate::observe::shared().latest();
    let claude = super::pane_claude(obs.session(sid)?)?;
    let (uuid, transcript, _) = pushed_session_for_pid(claude.row.pid)?;
    session_model(&uuid, Some(&transcript))
}

/// The model last observed on the account whose config directory is
/// `config_dir`: the newest hook record with a model id whose
/// transcript lives under that directory.
pub(in crate::plugins) fn account_model(config_dir: &Path) -> Option<String> {
    let mut best: Option<(SystemTime, String)> = None;
    for e in std::fs::read_dir(model_push_dir()).ok()?.flatten() {
        if e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(e.path()) else { continue };
        if !raw.lines().nth(1).is_some_and(|t| Path::new(t).starts_with(config_dir)) {
            continue;
        }
        let Some(id) = raw.lines().find_map(|l| l.strip_prefix("id=")).map(str::trim).filter(|id| cli_model_ok(id)) else {
            continue;
        };
        let at = e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(t, _)| at > *t) {
            best = Some((at, id.to_string()));
        }
    }
    best.map(|(_, id)| id)
}
