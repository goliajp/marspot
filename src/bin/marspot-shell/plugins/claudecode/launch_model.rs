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

/// The model the transcript's tail last names, as `--model` takes it:
/// the newest of an assistant record's id (`claude-opus-5-5`) and a
/// `/model` pick (`Opus 4.6 (1M context)` → `claude-opus-4-6[1m]`). An
/// error record's `<synthetic>` is not a model and is passed over.
pub(in crate::plugins) fn transcript_model(path: &Path) -> Option<String> {
    transcript_choice(path).model
}

/// [`transcript_model`] with the effort the same record ran at; a
/// `/model` pick says nothing about effort.
fn transcript_choice(path: &Path) -> super::CliChoice {
    use std::io::{Read, Seek, SeekFrom};
    let tail = || -> Option<String> {
        let mut f = std::fs::File::open(path).ok()?;
        let len = f.metadata().ok()?.len();
        f.seek(SeekFrom::Start(len.saturating_sub(super::model::MODEL_TAIL_BYTES))).ok()?;
        let mut raw = Vec::new();
        f.read_to_end(&mut raw).ok()?;
        Some(String::from_utf8_lossy(&raw).into_owned())
    };
    let text = tail().unwrap_or_default();
    text.lines().rev().find_map(line_choice).unwrap_or_default()
}

fn line_choice(l: &str) -> Option<super::CliChoice> {
    if let Some(name) = model_command_name(l) {
        // a pick with no id to name still ends the search: what came
        // before it is no longer the model
        return Some(super::CliChoice { model: picked_id(name), effort: None });
    }
    if !l.contains("\"role\":\"assistant\"") {
        return None;
    }
    let field = |key: &str| {
        let rest = &l[l.find(key)? + key.len()..];
        Some(&rest[..rest.find('"')?])
    };
    let id = field("\"model\":\"").filter(|id| cli_model_ok(id))?;
    let effort = field("\"effort\":\"").and_then(super::model::short_effort).filter(|e| super::model::cli_effort_ok(e));
    Some(super::CliChoice { model: Some(id.to_string()), effort })
}

/// The display name a `/model` output record in `line` switched to, as
/// claude printed it (escapes and remark included).
///
/// Records vary by claude version (`"subtype":"local_command"`, or a
/// user record holding the `<local-command-stdout>` block); the marker
/// is anchored as the direct value of a content field, quotes
/// unescaped, so conversation text that merely quotes it (escaped
/// quotes) cannot match. The name may carry a trailing remark ("… and
/// saved as your default for new sessions"), so the cut stops at " and ".
pub(super) fn model_command_name(line: &str) -> Option<&str> {
    ["\"content\":\"<local-command-stdout>Set model to ", "\"content\":\"<local-command-stdout>Kept model as "]
        .iter()
        .find_map(|marker| {
            let rest = &line[line.find(marker)? + marker.len()..];
            let mut end = rest.find(['<', '"']).unwrap_or(rest.len());
            if let Some(a) = rest.find(" and ") {
                end = end.min(a);
            }
            Some(&rest[..end])
        })
}

/// The id a `/model` display name stands for: `Fable 5.1` is
/// `claude-fable-5-1`, a `(1M context)` remark is `[1m]`. A name that is
/// not a family and a version (`Default`, `Opus 1M`) says nothing.
fn picked_id(name: &str) -> Option<String> {
    let short = super::model::short_model(name);
    let (family, version) = short.split_once('-')?;
    let numbered = !version.is_empty() && version.split('-').all(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()));
    if !["opus", "sonnet", "haiku", "fable"].contains(&family) || !numbered {
        return None;
    }
    let wide = if name.contains("(1M context)") { "[1m]" } else { "" };
    Some(format!("claude-{short}{wide}"))
}

/// The model and effort a bound session restarts with: the hook's
/// record, else (no `id=` line, as for a pane whose hook never ran)
/// what its transcript last names, the record's effort when the
/// transcript says none.
pub(super) fn bound_choice(meta: &super::BindMeta) -> super::CliChoice {
    let pushed = pushed_choice(&meta.uuid);
    if pushed.model.is_some() || !meta.has_transcript {
        return pushed;
    }
    let transcript = || {
        let home = meta.config_dir.as_deref()?;
        crate::plugins::handoff::history::claude_transcript(Path::new(home), &meta.uuid)
    };
    let read = transcript().map(|t| transcript_choice(&t)).unwrap_or_default();
    super::CliChoice { effort: read.effort.or(pushed.effort), ..read }
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
