//! The watch itself: which files, what each held before its last
//! change, and what is done when one changes.

use super::decide::{act, Act, Seen};
use super::*;
use marspot::fs_watch::Watcher;
use marspot::settings::CcModel;
use marspot::ui::strings::{t, text, Arg, Msg};
use marspot_peer::json::Value;

/// How long a reading that is not whole is given to become whole, and
/// how many times it is read again: a writer that truncates the file
/// and then writes it is seen in between.
const SETTLE: Duration = Duration::from_millis(50);
const SETTLE_TRIES: u32 = 4;

/// The settings files to watch, each by the path its links lead to.
///
/// The person's own files are watched only by an installed app.  A
/// sandbox and every test watch only the file `MARSPOT_TEST_CC_SETTINGS`
/// names, and nothing without it: a test or a sandbox must never edit
/// the settings every account shares.
pub(super) fn files() -> Vec<PathBuf> {
    let sandbox = cfg!(test)
        || std::env::var_os("MARSPOT_DEV_SANDBOX").is_some()
        || std::env::var_os("MARSPOT_STATE_DIR").is_some_and(|d| !d.is_empty());
    match std::env::var_os(ENV_TEST_FILE).map(PathBuf::from) {
        Some(p) if sandbox => vec![p.canonicalize().unwrap_or(p)],
        _ if sandbox => Vec::new(),
        _ => crate::plugins::claudecode::cc_settings_files(),
    }
}

/// One watched file and what it held before its last change.
pub(super) struct Entry {
    pub(super) path: PathBuf,
    pub(super) seen: Seen,
}

fn read(path: &Path) -> Seen {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).map_or(Seen::Unreadable, Seen::of_text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Seen::Absent,
        Err(_) => Seen::Unreadable,
    }
}

fn model_name(m: &Option<Value>) -> String {
    m.as_ref().map_or_else(|| t(Msg::ModelUnset).to_string(), |v| v.str().map_or_else(|| v.to_json(), str::to_string))
}

impl Entry {
    pub(super) fn of(path: PathBuf) -> Entry {
        watching(&path);
        let mut seen = read(&path);
        for _ in 0..SETTLE_TRIES {
            if seen.is_whole() || seen == Seen::Absent {
                break;
            }
            std::thread::sleep(SETTLE);
            seen = read(&path);
        }
        Entry { path, seen }
    }

    /// The file may have changed: look, and do what the change calls for.
    pub(super) fn look(&mut self) {
        let managed = marspot::settings::get().cc_model == CcModel::Managed;
        let mut now = read(&self.path);
        for _ in 0..SETTLE_TRIES {
            if act(&self.seen, &now, managed, false) != Act::Wait {
                break;
            }
            std::thread::sleep(SETTLE);
            now = read(&self.path);
        }
        let own = matches!(&now, Seen::Whole { text, .. } if take_own(&self.path, text));
        let file = self.path.display().to_string();
        match act(&self.seen, &now, managed, own) {
            Act::Nothing => {}
            Act::Wait => marspot::lx_info!("svc.cc_model.not_whole", "the shared Claude Code settings are not a whole text; nothing done", file = file.as_str()),
            Act::Adopt => self.seen = now,
            Act::Tell => {
                marspot::lx_info!("svc.cc_model.settings_changed", "the shared Claude Code settings changed", file = file.as_str());
                tell(t(Msg::ClaudeSettingsChanged).to_string());
                self.seen = now;
            }
            Act::Keep { from, to } => self.keep(now, &from, &to, "manual"),
            Act::PutBack { to, back } if take_confirmation(Instant::now()) => self.keep(now, &back, &to, "confirmed"),
            Act::PutBack { to, back } => self.put_back(now, &to, &back),
        }
    }

    fn keep(&mut self, now: Seen, from: &Option<Value>, to: &Option<Value>, why: &str) {
        let file = self.path.display().to_string();
        let (from, to) = (model_name(from), model_name(to));
        marspot::lx_info!(
            "svc.cc_model.changed",
            "the default model all Claude Code accounts share changed and is kept",
            file = file.as_str(),
            from = from.as_str(),
            to = to.as_str(),
            why = why
        );
        tell(text(Arg::ModelNowIs(&to)));
        self.seen = now;
    }

    /// Change the model in the file's text as it is now back to what it
    /// was, through the link, leaving every other key as it is now.
    fn put_back(&mut self, now: Seen, to: &Option<Value>, back: &Option<Value>) {
        let Seen::Whole { text: current, .. } = &now else { return };
        let file = self.path.display().to_string();
        let two = |s: &Seen| matches!(s, Seen::Whole { text, .. } if model_key::has_two_models(text));
        let edited = if two(&self.seen) || two(&now) {
            Err("more than one model key in the settings, and readers differ on which counts".to_string())
        } else {
            model_key::with_model(current, back.as_ref()).ok_or_else(|| "the model cannot be changed in the text alone".to_string())
        };
        let done = edited.and_then(|edited| {
            let written = crate::plugins::claudecode::write_through_symlink(&self.path, &edited);
            written.map(|()| edited).map_err(|e| e.to_string())
        });
        let (to_name, back_name) = (model_name(to), model_name(back));
        match done {
            Ok(edited) => {
                marspot::lx_info!(
                    "svc.cc_model.put_back",
                    "the default model all Claude Code accounts share changed without confirmation and was put back",
                    file = file.as_str(),
                    to = to_name.as_str(),
                    back = back_name.as_str()
                );
                tell(text(Arg::ModelPutBack { to: &to_name, back: &back_name }));
                self.seen = Seen::of_text(edited);
            }
            Err(e) => {
                marspot::lx_warn!("svc.cc_model.put_back_failed", &e, file = file.as_str(), to = to_name.as_str());
                tell(text(Arg::ModelNowIs(&to_name)));
                self.seen = now;
            }
        }
    }
}

/// The files being watched, until dropped.
pub(super) struct Watch {
    _watcher: Watcher,
}

impl Watch {
    pub(super) fn start(mut files: Vec<PathBuf>) -> Option<Watch> {
        files.retain(|f| f.parent().is_some_and(Path::is_dir));
        if files.is_empty() {
            marspot::lx_info!("svc.cc_model.unwatched", "no shared Claude Code settings file to watch");
            return None;
        }
        let mut entries: Vec<Entry> = files.iter().cloned().map(Entry::of).collect();
        let named = files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join(" ");
        let watcher = Watcher::spawn(&files, move || entries.iter_mut().for_each(Entry::look));
        match watcher {
            Ok(w) => {
                marspot::lx_info!("svc.cc_model.watching", "watching the shared Claude Code settings", files = named.as_str());
                Some(Watch { _watcher: w })
            }
            Err(e) => {
                marspot::lx_warn!("svc.cc_model.unwatched", &e.to_string(), files = named.as_str());
                None
            }
        }
    }
}
