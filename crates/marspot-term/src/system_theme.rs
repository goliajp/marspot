//! What the `system` theme setting resolves to, carried between
//! processes as one small file.
//!
//! Only the shell can ask the system which appearance is in force: it
//! is the process with an application object.  It writes the answer
//! here whenever the answer changes, whatever the setting says, so a
//! user who switches to `system` gets it within the same sweep.  The
//! core and every session read the file on the sweep that already
//! watches `settings.toml`, and only while the setting follows the
//! system: a named theme costs nothing here.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::palette::{self, ThemeId};

/// `<state>/system-theme`: one line, a theme id.
pub fn path() -> PathBuf {
    crate::paths::state_root().join("system-theme")
}

/// The terminal palette of the theme the settings name, for a caller
/// that may run before anything else has read the settings: the first
/// window is created in the right colour, not corrected a tick later.
pub fn startup_palette() -> &'static palette::Palette {
    let _ = crate::settings::get();
    palette::current()
}

/// Whether the theme in force has changed since this was last asked,
/// logging `THEME_CHANGED` when it has: every process says it the
/// same way, with `what` being what that process does about it.
pub fn changed(what: &str) -> bool {
    static SEEN: AtomicU32 = AtomicU32::new(0);
    let version = palette::version();
    if SEEN.swap(version, Ordering::Relaxed) == version {
        return false;
    }
    crate::lx_event!(
        "THEME_CHANGED",
        what,
        theme = palette::current_id().as_str(),
        by = if palette::follows_system() { "system" } else { "setting" }
    );
    true
}

/// The mtime of the file as last read, so an unchanged file is one
/// `stat` and no read.
static SEEN: Mutex<Option<SystemTime>> = Mutex::new(None);

/// Take the file's answer if the setting follows the system and the
/// file has changed since the last look.  A missing or unreadable file
/// leaves the last answer in force.
pub fn poll() {
    if !palette::follows_system() {
        return;
    }
    let now = std::fs::metadata(path()).and_then(|m| m.modified()).ok();
    let mut seen = SEEN.lock().unwrap_or_else(|p| p.into_inner());
    if now.is_none() || *seen == now {
        return;
    }
    *seen = now;
    if let Some(theme) = std::fs::read_to_string(path()).ok().and_then(|s| ThemeId::parse(&s)) {
        palette::set_system(theme);
    }
}

/// Publish what `system` resolves to: for this process at once, and
/// for the others through the file.  Atomic — `.tmp` + rename, as
/// every other state file does.
pub fn publish(theme: ThemeId) -> io::Result<()> {
    palette::set_system(theme);
    let p = path();
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = p.with_extension("tmp");
    std::fs::File::create(&tmp)?.write_all(format!("{}\n", theme.as_str()).as_bytes())?;
    std::fs::rename(&tmp, &p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvGuard;

    /// The file is read only while the setting follows the system, and
    /// what it says is in force by the next sweep.
    #[test]
    fn the_file_is_followed_only_when_the_setting_says_system() {
        let mut env = EnvGuard::new();
        let dir = std::env::temp_dir().join(format!("marspot-system-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        env.set("MARSPOT_STATE_DIR", &dir);

        palette::set_choice(Some(ThemeId::Dark));
        std::fs::write(path(), "light\n").unwrap();
        poll();
        assert_eq!(palette::current_id(), ThemeId::Dark, "a named theme does not read the file");

        palette::set_choice(None);
        poll();
        assert_eq!(palette::current_id(), ThemeId::Light);

        // Another process's write: a new file under the same name.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(path().with_extension("other"), "contrast\n").unwrap();
        std::fs::rename(path().with_extension("other"), path()).unwrap();
        poll();
        assert_eq!(palette::current_id(), ThemeId::Contrast);

        // A file that names no theme changes nothing.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(path(), "sepia\n").unwrap();
        poll();
        assert_eq!(palette::current_id(), ThemeId::Contrast);

        publish(ThemeId::Dark).unwrap();
        assert_eq!(palette::current_id(), ThemeId::Dark, "publishing takes effect here at once");
        assert_eq!(std::fs::read_to_string(path()).unwrap(), "dark\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
