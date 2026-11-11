//! What the `system` language setting resolves to, carried between
//! processes as one small file.
//!
//! Only the shell can ask the system for the user's language list, and
//! only the shell knows which language the system's own menu items came
//! up in at launch.  It writes both here; the core reads the file on
//! the sweep that already watches `settings.toml`.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::language::{self, LangId};

/// `<state>/system-language`: the language `system` resolves to, then
/// the language the system's own items were launched in.
pub fn path() -> PathBuf {
    crate::paths::state_root().join("system-language")
}

/// Whether the language in force has changed since this was last
/// asked, logging `LANGUAGE_CHANGED` when it has, with `what` being
/// what this process does about it.
pub fn changed(what: &str) -> bool {
    static SEEN: AtomicU32 = AtomicU32::new(0);
    let version = language::version();
    if SEEN.swap(version, Ordering::Relaxed) == version {
        return false;
    }
    crate::lx_event!(
        "LANGUAGE_CHANGED",
        what,
        lang = language::current_id().as_str(),
        by = if language::follows_system() { "system" } else { "setting" }
    );
    true
}

/// The language the system's own menu items are in until the next
/// launch; `u8::MAX` while unknown.
static LAUNCHED: AtomicU8 = AtomicU8::new(u8::MAX);

/// The language the items the system provides were launched in, when
/// the shell has said.
pub fn launched_in() -> Option<LangId> {
    LangId::ALL.get(LAUNCHED.load(Ordering::Relaxed) as usize).copied()
}

/// The mtime of the file as last read, so an unchanged file is one
/// `stat` and no read.
static SEEN: Mutex<Option<SystemTime>> = Mutex::new(None);

/// Take the file's answer if it has changed since the last look.  A
/// missing or unreadable file leaves the last answer in force.
pub fn poll() {
    let now = std::fs::metadata(path()).and_then(|m| m.modified()).ok();
    let mut seen = SEEN.lock().unwrap_or_else(|p| p.into_inner());
    if now.is_none() || *seen == now {
        return;
    }
    *seen = now;
    let Ok(body) = std::fs::read_to_string(path()) else { return };
    let mut lines = body.lines();
    if let Some(lang) = lines.next().and_then(LangId::parse) {
        language::set_system(lang);
    }
    if let Some(lang) = lines.next().and_then(LangId::parse) {
        LAUNCHED.store(lang as u8, Ordering::Relaxed);
    }
}

/// Publish what `system` resolves to and what the system's own items
/// were launched in: for this process at once, and for the others
/// through the file.  Atomic — `.tmp` + rename.
pub fn publish(system: LangId, launched: LangId) -> io::Result<()> {
    language::set_system(system);
    LAUNCHED.store(launched as u8, Ordering::Relaxed);
    let p = path();
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = p.with_extension("tmp");
    std::fs::File::create(&tmp)?
        .write_all(format!("{}\n{}\n", system.as_str(), launched.as_str()).as_bytes())?;
    std::fs::rename(&tmp, &p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvGuard;

    #[test]
    fn the_file_carries_the_system_language_and_the_launch_language() {
        let mut env = EnvGuard::new();
        let dir = std::env::temp_dir().join(format!("marspot-system-language-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        env.set("MARSPOT_STATE_DIR", &dir);

        language::set_choice(Some(LangId::En));
        std::fs::write(path(), "ja\nen\n").unwrap();
        poll();
        assert_eq!(language::current_id(), LangId::En, "a named language is not overridden");
        assert_eq!(launched_in(), Some(LangId::En));

        language::set_choice(None);
        assert_eq!(language::current_id(), LangId::Ja, "and the file's answer was remembered");

        // a file that names no language changes nothing
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(path(), "tlh\n").unwrap();
        poll();
        assert_eq!(language::current_id(), LangId::Ja);

        publish(LangId::ZhHans, LangId::Ja).unwrap();
        assert_eq!(language::current_id(), LangId::ZhHans, "publishing takes effect here at once");
        assert_eq!(launched_in(), Some(LangId::Ja));
        assert_eq!(std::fs::read_to_string(path()).unwrap(), "zh-Hans\nja\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
