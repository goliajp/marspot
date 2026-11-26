//! The settings file every Claude Code account shares, watched.
//!
//! Each `~/.claude-profile-N/settings.json` links to one file, so a
//! `/model` in one account's pane writes the default model of all of
//! them.  A kqueue watch on that file (and its directory, for writers
//! that replace it by rename) keeps the text before each change in
//! memory.  When the `model` key appears or changes it is logged and
//! the person at the window is told within a second.  Under
//! `claude.model = managed`, a change no `/model` confirmed at the
//! keyboard is undone by writing the previous text back through the
//! link; a confirmed one is kept.  Any other change is only told.
//!
//! The watch runs on threads of its own and only when a write wakes it:
//! with the file quiet it costs nothing.

mod decide;
mod watch;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long after a `/model` confirmed at the keyboard the next change
/// of model counts as that one: picking from `/model`'s own list takes
/// a while.
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
}

static SHARED: Mutex<Shared> = Mutex::new(Shared { confirmed_at: None, notices: VecDeque::new(), own: VecDeque::new() });

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
    if s.own.len() == OWN_CAP {
        s.own.pop_front();
    }
    s.own.push_back((path.to_path_buf(), hash(text)));
}

/// Whether `text` in `path` is what marspot last wrote there.
fn is_own(path: &Path, text: Option<&str>) -> bool {
    let Some(text) = text else { return false };
    let h = hash(text);
    shared().own.iter().rev().find(|(p, _)| p == path).is_some_and(|(_, own)| *own == h)
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
}

#[cfg(test)]
mod tests;
