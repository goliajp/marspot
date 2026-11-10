//! Which model a gemini session is set to, and how the badge names it.
//!
//! gemini has two things called the model.  The one a session is *set
//! to* is what `-m`, `GEMINI_MODEL`, `model.name` in the settings or
//! `/model set` named: often an alias, and `auto` when nothing named
//! one.  It is what gemini's footer shows and what a relaunch has to be
//! given.  The one a reply *ran on* is concrete — the router, the alias
//! and any fallback already applied — and is recorded with the reply.
//!
//! The badge shows the second once there is a reply, behind `Auto→`
//! when the session is set to the router, and the first before that.
//! gemini has no reasoning-effort setting to show beside it: the
//! thinking level is a per-model default built into the CLI, and
//! nothing on disk says what a request was sent with.
//!
//! Whatever cannot be read exactly is left unsaid.  The setting a
//! process started with is resolved the way gemini resolves it and is
//! given up on as soon as one input is out of reach: a `.env` file that
//! names the variable, a settings file with comments or placeholders,
//! one written after the process started.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use marspot_peer::json;

use super::session::Said;
use crate::plugins::pidtree::ProcId;

/// What gemini is set to when nothing names a model.
const AUTO: &str = "auto";

/// A model name that can be shown and typed after `-m`.  Narrower than
/// what gemini accepts, which is any string; a name outside this is
/// treated as not known rather than quoted.
pub(crate) fn is_model(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 96
        && !s.starts_with('-')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'/' | b':'))
}

/// Whether one source names a model.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) enum Named {
    #[default]
    Absent,
    As(String),
    /// It says something this code cannot read exactly.
    Unknown,
}

/// Everything the setting a process starts with depends on.
pub(super) struct Launch {
    /// `-m` / `--model`.
    pub flag: Named,
    /// `GEMINI_MODEL` in the process's environment.
    pub env: Option<String>,
    /// The home gemini runs under.
    pub root: PathBuf,
    pub cwd: Option<PathBuf>,
    pub started: SystemTime,
    pub system_settings: PathBuf,
    pub system_defaults: PathBuf,
}

/// The two machine-wide settings files, which the environment may move.
pub(super) fn system_paths(settings: Option<String>, defaults: Option<String>) -> (PathBuf, PathBuf) {
    let set = |v: Option<String>| v.filter(|v| !v.is_empty()).map(PathBuf::from);
    let settings = set(settings).unwrap_or_else(|| PathBuf::from("/Library/Application Support/GeminiCli/settings.json"));
    let defaults = set(defaults).unwrap_or_else(|| settings.with_file_name("system-defaults.json"));
    (settings, defaults)
}

/// `model.name` in one settings file.
fn named_in(text: &str) -> Named {
    let Ok(settings) = json::parse(text) else {
        // gemini strips comments before parsing; this does not
        return if text.contains("model") { Named::Unknown } else { Named::Absent };
    };
    let Some(model) = settings.get("model") else { return Named::Absent };
    let json::Value::Obj(fields) = model else { return Named::Unknown };
    match fields.iter().find(|(k, _)| k == "name").map(|(_, v)| v) {
        None => Named::Absent,
        // `$VAR` is expanded from an environment that includes `.env`
        Some(json::Value::Str(name)) if is_model(name.trim()) => Named::As(name.trim().to_string()),
        Some(_) => Named::Unknown,
    }
}

/// `model.name` in the settings file at `path`, as the process read it.
fn layer(path: &Path, started: SystemTime) -> Named {
    let Ok(meta) = std::fs::metadata(path) else { return Named::Absent };
    // written since: what the process read is no longer there to see
    if !meta.modified().is_ok_and(|written| written <= started) {
        return Named::Unknown;
    }
    std::fs::read_to_string(path).map_or(Named::Unknown, |text| named_in(&text))
}

/// No `.env` file gemini could load names the variable.  gemini loads
/// the first it finds walking up from the directory, then the home's;
/// which one that is depends on folder trust, so all are looked at.
fn dotenv_is_silent(cwd: &Path, root: &Path) -> bool {
    cwd.ancestors().chain(std::iter::once(root)).flat_map(|d| [d.join(".gemini/.env"), d.join(".env")]).all(|file| {
        match std::fs::read(&file) {
            Ok(text) => !text.windows(12).any(|w| w == b"GEMINI_MODEL"),
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        }
    })
}

/// The model a process was set to as it started: the flag, then the
/// environment, then the settings, then `auto` — gemini's own order.
/// `None` when any input that could decide it cannot be read exactly.
///
/// Reads files under the working directory, which may be on a network
/// mount: not for the main thread.
pub(super) fn started_with(l: &Launch) -> Option<String> {
    match &l.flag {
        Named::As(m) => return Some(m.clone()),
        Named::Unknown => return None,
        Named::Absent => {}
    }
    // an empty value is unset to gemini; one of only spaces is a model
    // named "", which it replaces with the default
    if let Some(v) = l.env.as_deref().filter(|v| !v.is_empty()) {
        let v = v.trim();
        return if v.is_empty() { Some(AUTO.to_string()) } else { is_model(v).then(|| v.to_string()) };
    }
    let cwd = l.cwd.as_deref()?;
    if !dotenv_is_silent(cwd, &l.root) {
        return None;
    }
    // a machine-wide file is skipped when its permissions are loose,
    // which is not checked here: one that names a model decides nothing
    if layer(&l.system_settings, l.started) != Named::Absent || layer(&l.system_defaults, l.started) != Named::Absent {
        return None;
    }
    let user = match layer(&super::config_dir(&l.root).join("settings.json"), l.started) {
        Named::As(m) => m,
        Named::Absent => AUTO.to_string(),
        Named::Unknown => return None,
    };
    // the project's file outranks the user's, but only in a trusted
    // folder; it is believed when both say the same
    let same = |p: &Path| std::fs::canonicalize(p).ok();
    let project = if same(cwd) == same(&l.root) {
        Named::Absent
    } else {
        layer(&super::config_dir(cwd).join("settings.json"), l.started)
    };
    match project {
        Named::Absent => Some(user),
        Named::As(m) if m == user => Some(user),
        _ => None,
    }
}

/// What each gemini process started with, resolved once per process
/// on a thread of its own and kept for as long as the process lives.
#[derive(Default)]
pub(super) struct Starts {
    found: HashMap<ProcId, Arc<OnceLock<Option<String>>>>,
}

impl Starts {
    pub(super) fn retain(&mut self, live: &[ProcId]) {
        self.found.retain(|id, _| live.contains(id));
    }

    pub(super) fn has(&self, proc: ProcId) -> bool {
        self.found.contains_key(&proc)
    }

    /// Start resolving `proc`'s setting.  A thread that cannot be
    /// started leaves it unknown.
    pub(super) fn resolve(&mut self, proc: ProcId, launch: Launch) {
        let slot = Arc::new(OnceLock::new());
        self.found.insert(proc, Arc::clone(&slot));
        let _ = std::thread::Builder::new().name("gemini-model".into()).spawn(move || {
            let _ = slot.set(started_with(&launch));
        });
    }

    /// What `proc` started with; `None` while that is being worked out
    /// and when it could not be.
    pub(super) fn of(&self, proc: ProcId) -> Option<&str> {
        self.found.get(&proc)?.get()?.as_deref()
    }
}

/// The model the session is set to now: what it started with, unless
/// the session file records a change since.
fn set_to(started: Option<&str>, said: Option<&Said>) -> Option<String> {
    match said {
        Some(s) if s.unreadable => None,
        Some(s) => s.fallback.as_deref().or(s.chosen.as_deref()).or(started).map(str::to_string),
        None => started.map(str::to_string),
    }
}

/// The model to start this session's next process with, so that an
/// account switch or a handoff back does not quietly change it.  What
/// the user chose: a fallback forced by one account's quota is not
/// carried to the next account.
pub(super) fn carried(started: Option<&str>, said: Option<&Said>) -> Option<String> {
    match said {
        Some(s) if s.unreadable => None,
        Some(s) => s.chosen.as_deref().or(started).map(str::to_string),
        None => started.map(str::to_string),
    }
}

/// gemini's own name for a router setting.
fn router_name(model: &str) -> Option<&'static str> {
    match model {
        "auto" => Some("Auto"),
        "auto-gemini-3" => Some("Auto (Gemini 3)"),
        "auto-gemini-2.5" => Some("Auto (Gemini 2.5)"),
        _ => None,
    }
}

/// A concrete model as gemini's footer writes it.
fn as_shown(model: &str) -> &str {
    match model {
        "gemini-3.1-pro-preview-customtools" => "gemini-3.1-pro-preview",
        other => other,
    }
}

/// The badge's model: `Auto→gemini-3-flash-preview` for a routed
/// session that has replied, the reply's model for any other, and the
/// setting alone before there is a reply under it.
pub(super) fn shown(started: Option<&str>, said: Option<&Said>) -> Option<String> {
    let set = set_to(started, said);
    let router = set.as_deref().and_then(router_name);
    match (said.and_then(|s| s.reply.as_deref()), router) {
        (Some(reply), Some(router)) => Some(format!("{router}→{}", as_shown(reply))),
        (Some(reply), None) => Some(as_shown(reply).to_string()),
        (None, Some(router)) => Some(router.to_string()),
        (None, None) => set,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::handoff::history::tmp;
    use std::time::Duration;

    /// A home and a project under it, with the process started now, so
    /// every file written before this returns was there for it to read.
    fn launch(name: &str) -> Launch {
        let base = tmp(name);
        let (root, cwd) = (base.join("home"), base.join("work/a"));
        std::fs::create_dir_all(root.join(".gemini")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        Launch {
            flag: Named::Absent,
            env: None,
            root,
            cwd: Some(cwd),
            started: SystemTime::now() + Duration::from_secs(60),
            system_settings: base.join("system/settings.json"),
            system_defaults: base.join("system/system-defaults.json"),
        }
    }

    fn user_settings(l: &Launch, text: &str) {
        std::fs::write(l.root.join(".gemini/settings.json"), text).unwrap();
    }

    #[test]
    fn nothing_named_is_the_router() {
        let l = launch("gemini-model-default");
        assert_eq!(started_with(&l).as_deref(), Some("auto"));
        user_settings(&l, r#"{"privacy":{"usageStatisticsEnabled":false}}"#);
        assert_eq!(started_with(&l).as_deref(), Some("auto"), "settings that name no model");
    }

    #[test]
    fn the_flag_outranks_the_environment_and_the_environment_the_settings() {
        let mut l = launch("gemini-model-order");
        user_settings(&l, r#"{"model":{"name":"pro"}}"#);
        assert_eq!(started_with(&l).as_deref(), Some("pro"));
        l.env = Some("gemini-2.5-flash".into());
        assert_eq!(started_with(&l).as_deref(), Some("gemini-2.5-flash"));
        l.flag = Named::As("gemini-2.5-pro".into());
        assert_eq!(started_with(&l).as_deref(), Some("gemini-2.5-pro"));
        l.flag = Named::Unknown;
        assert_eq!(started_with(&l), None, "a flag that could not be read decides, and is not known");
    }

    #[test]
    fn the_environment_is_read_as_gemini_reads_it() {
        let mut l = launch("gemini-model-env");
        user_settings(&l, r#"{"model":{"name":"pro"}}"#);
        l.env = Some(String::new());
        assert_eq!(started_with(&l).as_deref(), Some("pro"), "empty is unset");
        l.env = Some("  ".into());
        assert_eq!(started_with(&l).as_deref(), Some("auto"), "blank is a model named nothing: the default");
        l.env = Some(" flash ".into());
        assert_eq!(started_with(&l).as_deref(), Some("flash"));
        l.env = Some("two words".into());
        assert_eq!(started_with(&l), None);
    }

    #[test]
    fn settings_that_cannot_be_read_exactly_decide_nothing() {
        let l = launch("gemini-model-settings");
        for text in [
            "{\n  // my model\n  \"model\": {\"name\": \"pro\"}\n}",
            r#"{"model":{"name":"$MY_MODEL"}}"#,
            r#"{"model":"gemini-2.5-pro"}"#,
            r#"{"model":{"name":""}}"#,
        ] {
            user_settings(&l, text);
            assert_eq!(started_with(&l), None, "{text}");
        }
        user_settings(&l, "{\n  // only the theme\n  \"ui\": {}\n}");
        assert_eq!(started_with(&l).as_deref(), Some("auto"), "comments, but nothing about a model");
        user_settings(&l, r#"{"model":{"maxSessionTurns":9}}"#);
        assert_eq!(started_with(&l).as_deref(), Some("auto"));

        // written after the process started: not what it read
        user_settings(&l, r#"{"model":{"name":"pro"}}"#);
        let early = Launch { started: SystemTime::now() - Duration::from_secs(60), ..launch("gemini-model-early") };
        user_settings(&early, r#"{"model":{"name":"pro"}}"#);
        assert_eq!(started_with(&early), None);
    }

    #[test]
    fn a_project_file_is_believed_only_when_it_agrees() {
        let l = launch("gemini-model-project");
        let project = l.cwd.as_ref().unwrap().join(".gemini");
        std::fs::create_dir_all(&project).unwrap();
        user_settings(&l, r#"{"model":{"name":"pro"}}"#);
        std::fs::write(project.join("settings.json"), r#"{"model":{"name":"pro"}}"#).unwrap();
        assert_eq!(started_with(&l).as_deref(), Some("pro"));
        std::fs::write(project.join("settings.json"), r#"{"model":{"name":"flash"}}"#).unwrap();
        assert_eq!(started_with(&l), None, "whether it applies depends on folder trust");
        std::fs::write(project.join("settings.json"), r#"{"ui":{}}"#).unwrap();
        assert_eq!(started_with(&l).as_deref(), Some("pro"));

        // gemini run in its own home: that file is the user's, read once
        let mut home = launch("gemini-model-home");
        home.cwd = Some(home.root.clone());
        user_settings(&home, r#"{"model":{"name":"flash"}}"#);
        assert_eq!(started_with(&home).as_deref(), Some("flash"));
    }

    #[test]
    fn a_machine_wide_file_or_a_dotenv_that_names_one_decides_nothing() {
        let l = launch("gemini-model-system");
        std::fs::create_dir_all(l.system_settings.parent().unwrap()).unwrap();
        std::fs::write(&l.system_settings, r#"{"model":{"name":"pro"}}"#).unwrap();
        assert_eq!(started_with(&l), None);
        std::fs::write(&l.system_settings, r#"{"ui":{}}"#).unwrap();
        assert_eq!(started_with(&l).as_deref(), Some("auto"));

        let above = l.cwd.as_ref().unwrap().parent().unwrap().join(".env");
        std::fs::write(&above, "OTHER=1\n").unwrap();
        assert_eq!(started_with(&l).as_deref(), Some("auto"));
        std::fs::write(&above, "GEMINI_MODEL=flash\n").unwrap();
        assert_eq!(started_with(&l), None, "a file above the directory");
        std::fs::remove_file(&above).unwrap();
        std::fs::write(l.root.join(".gemini/.env"), "GEMINI_MODEL=flash\n").unwrap();
        assert_eq!(started_with(&l), None, "the home's own");
        // the flag still decides: the file is only read when nothing else names one
        assert_eq!(started_with(&Launch { flag: Named::As("pro".into()), ..l }).as_deref(), Some("pro"));
    }

    #[test]
    fn the_machine_wide_paths_follow_the_environment() {
        let (s, d) = system_paths(None, Some(String::new()));
        assert_eq!(s, Path::new("/Library/Application Support/GeminiCli/settings.json"));
        assert_eq!(d, Path::new("/Library/Application Support/GeminiCli/system-defaults.json"));
        let (s, d) = system_paths(Some("/etc/g/settings.json".into()), None);
        assert_eq!((s.as_path(), d.as_path()), (Path::new("/etc/g/settings.json"), Path::new("/etc/g/system-defaults.json")));
        assert_eq!(system_paths(None, Some("/x/d.json".into())).1, Path::new("/x/d.json"));
    }

    fn said(chosen: Option<&str>, fallback: Option<&str>, reply: Option<&str>) -> Said {
        Said { chosen: chosen.map(Into::into), fallback: fallback.map(Into::into), reply: reply.map(Into::into), unreadable: false }
    }

    #[test]
    fn the_badge_names_the_setting_until_a_reply_names_the_model() {
        assert_eq!(shown(Some("auto"), None).as_deref(), Some("Auto"), "no session file yet");
        assert_eq!(shown(Some("pro"), Some(&said(None, None, None))).as_deref(), Some("pro"), "an alias stays one");
        assert_eq!(
            shown(Some("auto"), Some(&said(None, None, Some("gemini-3-flash-preview")))).as_deref(),
            Some("Auto→gemini-3-flash-preview")
        );
        assert_eq!(
            shown(Some("auto-gemini-2.5"), Some(&said(None, None, Some("gemini-2.5-pro")))).as_deref(),
            Some("Auto (Gemini 2.5)→gemini-2.5-pro")
        );
        assert_eq!(shown(Some("pro"), Some(&said(None, None, Some("gemini-3.1-pro-preview")))).as_deref(), Some("gemini-3.1-pro-preview"));
        assert_eq!(
            shown(Some("auto"), Some(&said(None, None, Some("gemini-3.1-pro-preview-customtools")))).as_deref(),
            Some("Auto→gemini-3.1-pro-preview"),
            "written as gemini's footer writes it"
        );
        assert_eq!(shown(None, Some(&said(None, None, Some("gemini-2.5-pro")))).as_deref(), Some("gemini-2.5-pro"), "the reply is known even when the setting is not");
        assert_eq!(shown(None, Some(&said(None, None, None))), None);
        assert_eq!(shown(None, None), None);
    }

    #[test]
    fn a_recorded_change_replaces_what_the_process_started_with() {
        // `/model set`, no reply under it yet
        let set = said(Some("gemini-2.5-flash"), None, None);
        assert_eq!(shown(Some("auto"), Some(&set)).as_deref(), Some("gemini-2.5-flash"));
        assert_eq!(carried(Some("auto"), Some(&set)).as_deref(), Some("gemini-2.5-flash"));
        // back to the router
        let routed = said(Some("auto"), None, Some("gemini-3-pro-preview"));
        assert_eq!(shown(Some("gemini-2.5-pro"), Some(&routed)).as_deref(), Some("Auto→gemini-3-pro-preview"));
        // a quota fallback: shown, and no longer routed, but not what the next account is started with
        let fell = said(None, Some("gemini-3-flash-preview"), None);
        assert_eq!(shown(Some("auto"), Some(&fell)).as_deref(), Some("gemini-3-flash-preview"));
        let fell = said(None, Some("gemini-3-flash-preview"), Some("gemini-3-flash-preview"));
        assert_eq!(shown(Some("auto"), Some(&fell)).as_deref(), Some("gemini-3-flash-preview"));
        assert_eq!(carried(Some("auto"), Some(&fell)).as_deref(), Some("auto"));
        assert_eq!(carried(None, Some(&fell)), None, "nothing known to carry");
        // a change to a name that cannot be read
        let lost = Said { unreadable: true, ..said(Some("pro"), None, None) };
        assert_eq!(shown(Some("auto"), Some(&lost)), None);
        assert_eq!(carried(Some("auto"), Some(&lost)), None);
        let lost = Said { unreadable: true, ..said(None, None, Some("gemini-2.5-pro")) };
        assert_eq!(shown(Some("auto"), Some(&lost)).as_deref(), Some("gemini-2.5-pro"), "the reply still says what ran");
    }

    #[test]
    fn a_setting_is_resolved_once_and_forgotten_with_its_process() {
        let mut starts = Starts::default();
        let proc = ProcId { pid: 7, start_us: 7 };
        assert!(!starts.has(proc));
        starts.resolve(proc, Launch { flag: Named::As("flash".into()), ..launch("gemini-model-starts") });
        assert!(starts.has(proc));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while starts.of(proc).is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(starts.of(proc), Some("flash"));
        starts.retain(&[]);
        assert!(!starts.has(proc) && starts.found.is_empty());
    }

    #[test]
    fn a_model_name_is_one_word_of_plain_characters() {
        for ok in ["flash", "gemini-3.1-pro-preview-customtools", "publishers/google/models/gemini-2.5-pro", "tuned:v1_2"] {
            assert!(is_model(ok), "{ok}");
        }
        for bad in ["", "-m", "two words", "x;rm", "a'b", "$(x)", &"m".repeat(97)] {
            assert!(!is_model(bad), "{bad}");
        }
    }
}
