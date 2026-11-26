//! The settings file every Claude Code account shares, watched.
//!
//! Each `~/.claude-profile-N/settings.json` links to one file, so a
//! `/model` in one account's pane writes the default model of all of
//! them.  A kqueue watch on that file (and its directory, for writers
//! that replace it by rename) keeps the text before each change in
//! memory.  When the `model` key appears or changes it is logged and
//! the person at the window is told within a second.  Under
//! `claude.model = managed`, a change no `/model` confirmed at the
//! keyboard is undone by setting that one key back in the file's
//! current text, through the link; a confirmed one is kept.  Any other
//! change is only told.
//!
//! The file is never removed, and nothing is done from a reading that
//! is not a whole JSON object (an unreadable file, a missing one, or a
//! writer caught between truncating and writing): such a reading is
//! read again for a moment, and if it does not become whole the last
//! whole one stays the one to go back to.
//!
//! Only an installed app watches the person's files.  Tests and
//! sandboxes watch only the file `MARSPOT_TEST_CC_SETTINGS` names (see
//! `watch::files`), so starting the plugin in a test touches nothing.
//!
//! The watch runs on threads of its own and only when a write wakes it:
//! with the file quiet it costs nothing.

mod decide;
mod model_key;
mod watch;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long after a `/model` confirmed at the keyboard the next change
/// of model counts as that one.  The time starts at the confirming
/// Enter, and the person may then spend a while in `/model`'s list, so
/// it is a minute.  It ends sooner on the first change of model the
/// watch sees, kept or put back: one confirmation is good for one write.
///
/// It does not end when the list is closed without a choice.  The core
/// could look for Claude Code's box coming back, but that redraw and
/// the settings write of a choice race each other, so ending on it
/// could put back the very model the person just confirmed; a list left
/// with Esc therefore leaves the confirmation open for the rest of the
/// minute.
pub(crate) const CONFIRMED_FOR: Duration = Duration::from_secs(60);

/// The most notices waiting for the main thread, and own writes
/// remembered.
const NOTICES_CAP: usize = 8;
const OWN_CAP: usize = 16;

/// The variable naming the settings file to watch in place of the
/// shared one, honoured only in a sandbox.
pub(crate) const ENV_TEST_FILE: &str = "MARSPOT_TEST_CC_SETTINGS";

#[derive(Default)]
struct Shared {
    confirmed_at: Option<Instant>,
    notices: VecDeque<String>,
    /// Text marspot itself wrote, by file: the watch sees those writes
    /// too and says nothing about them.
    own: VecDeque<(PathBuf, u64)>,
    /// The files being watched, by the path the watch reads: writes to
    /// any other file (the other agents' configs) are not remembered,
    /// so they cannot push a watched file's own write out of `own`.
    watched: Vec<PathBuf>,
}

static SHARED: Mutex<Shared> =
    Mutex::new(Shared { confirmed_at: None, notices: VecDeque::new(), own: VecDeque::new(), watched: Vec::new() });

fn shared() -> std::sync::MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|p| p.into_inner())
}

fn hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// The keyboard confirmed a `/model` in pane `sid`, and it was sent.
pub(crate) fn confirmed(sid: u64) {
    shared().confirmed_at = Some(Instant::now());
    marspot::lx_info!("svc.cc_model.confirmed", "a /model was confirmed at the keyboard", sid = sid);
}

/// Takes the confirmation when it is recent enough to count.
fn take_confirmation(now: Instant) -> bool {
    let mut s = shared();
    let fresh = s.confirmed_at.is_some_and(|t| now.duration_since(t) <= CONFIRMED_FOR);
    s.confirmed_at = None;
    fresh
}

/// Marspot is about to write `text` to `path`.
pub(crate) fn note_own_write(path: &Path, text: &str) {
    let mut s = shared();
    if !s.watched.iter().any(|p| p == path) {
        return;
    }
    if s.own.len() == OWN_CAP {
        s.own.pop_front();
    }
    s.own.push_back((path.to_path_buf(), hash(text)));
}

/// Whether `text` in `path` is one marspot wrote there; that write is
/// then no longer remembered, so the same text from another writer
/// later is not taken for it.
fn take_own(path: &Path, text: &str) -> bool {
    let h = hash(text);
    let mut s = shared();
    let at = s.own.iter().rposition(|(p, own)| p == path && *own == h);
    at.and_then(|i| s.own.remove(i)).is_some()
}

fn tell(text: String) {
    {
        let mut s = shared();
        if s.notices.len() == NOTICES_CAP {
            s.notices.pop_front();
        }
        s.notices.push_back(text);
    }
    crate::pump::knock();
}

/// What the watch has to tell the person at the window, oldest first.
pub(crate) fn take_notices() -> Vec<String> {
    shared().notices.drain(..).collect()
}

static WATCH: Mutex<Option<watch::Watch>> = Mutex::new(None);

/// Start watching, off the calling thread: finding the files reads the
/// home directory.
pub(crate) fn start() {
    let spawned = std::thread::Builder::new().name("cc-model-watch-start".into()).spawn(|| {
        let files = watch::files();
        let w = watch::Watch::start(files);
        *WATCH.lock().unwrap_or_else(|p| p.into_inner()) = w;
    });
    if let Err(e) = spawned {
        marspot::lx_warn!("svc.cc_model.unwatched", &e.to_string());
    }
}

/// Stop watching.
pub(crate) fn stop() {
    WATCH.lock().unwrap_or_else(|p| p.into_inner()).take();
    let mut s = shared();
    s.watched.clear();
    s.own.clear();
}

/// `path` is watched from now on.
fn watching(path: &Path) {
    let mut s = shared();
    if !s.watched.iter().any(|p| p == path) {
        s.watched.push(path.to_path_buf());
    }
}

#[cfg(test)]
mod tests;
