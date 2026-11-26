//! The watch itself: which files, what each held before its last
//! change, and what is done when one changes.

use super::decide::{act, Act};
use super::*;
use marspot::fs_watch::Watcher;
use marspot::settings::CcModel;
use marspot::ui::strings::{t, text, Arg, Msg};

/// The settings files to watch, each by the path its links lead to.
///
/// A sandbox watches only the file `MARSPOT_TEST_CC_SETTINGS` names, and
/// nothing without it: it must never put back a file the installed app
/// was told to keep.
pub(super) fn files() -> Vec<PathBuf> {
    let sandbox = std::env::var_os("MARSPOT_DEV_SANDBOX").is_some()
        || std::env::var_os("MARSPOT_STATE_DIR").is_some_and(|d| !d.is_empty());
    match std::env::var_os(ENV_TEST_FILE).map(PathBuf::from) {
        Some(p) if sandbox => vec![p.canonicalize().unwrap_or(p)],
        _ if sandbox => Vec::new(),
        _ => crate::plugins::claudecode::cc_settings_files(),
    }
}

/// One watched file and the text it held before its last change.
pub(super) struct Entry {
    pub(super) path: PathBuf,
    pub(super) text: Option<String>,
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn model_name(m: &Option<String>) -> &str {
    m.as_deref().unwrap_or_else(|| t(Msg::ModelUnset))
}

impl Entry {
    pub(super) fn of(path: PathBuf) -> Entry {
        let text = read(&path);
        Entry { path, text }
    }

    /// The file may have changed: look, and do what the change calls for.
    pub(super) fn look(&mut self) {
        let now = read(&self.path);
        let managed = marspot::settings::get().cc_model == CcModel::Managed;
        let own = is_own(&self.path, now.as_deref());
        let file = self.path.display().to_string();
        match act(self.text.as_deref(), now.as_deref(), managed, false, own) {
            Act::Nothing if own => self.text = now,
            Act::Nothing => {}
            Act::Tell => {
                marspot::lx_info!("svc.cc_model.settings_changed", "the shared Claude Code settings changed", file = file.as_str());
                tell(t(Msg::ClaudeSettingsChanged).to_string());
                self.text = now;
            }
            Act::Keep { from, to } => self.keep(now, from, to, "manual"),
            Act::PutBack { to, back } if take_confirmation(Instant::now()) => self.keep(now, back, to, "confirmed"),
            Act::PutBack { to, back } => self.put_back(&to, &back),
        }
    }

    fn keep(&mut self, now: Option<String>, from: Option<String>, to: Option<String>, why: &str) {
        let file = self.path.display().to_string();
        marspot::lx_info!(
            "svc.cc_model.changed",
            "the default model all Claude Code accounts share changed and is kept",
            file = file.as_str(),
            from = model_name(&from),
            to = model_name(&to),
            why = why
        );
        tell(text(Arg::ModelNowIs(model_name(&to))));
        self.text = now;
    }

    /// Write the text before the change back, through the link.
    fn put_back(&mut self, to: &Option<String>, back: &Option<String>) {
        let file = self.path.display().to_string();
        let done = match &self.text {
            Some(before) => {
                note_own_write(&self.path, before);
                crate::plugins::claudecode::write_through_symlink(&self.path, before)
            }
            None => std::fs::remove_file(&self.path),
        };
        match done {
            Ok(()) => marspot::lx_info!(
                "svc.cc_model.put_back",
                "the default model all Claude Code accounts share changed without confirmation and was put back",
                file = file.as_str(),
                to = model_name(to),
                back = model_name(back)
            ),
            Err(e) => marspot::lx_warn!("svc.cc_model.put_back_failed", &e.to_string(), file = file.as_str()),
        }
        tell(text(Arg::ModelPutBack { to: model_name(to), back: model_name(back) }));
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
