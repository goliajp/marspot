//! When the agents' configuration is due another look.
//!
//! marspot keeps entries of its own in every agent's configuration: the
//! status-line hook and the pane-name hooks in Claude Code's settings,
//! the peer server in codex's, gemini's and kimi's, the skill in each
//! one's skills directory.  While the supervisor's clock runs they are
//! looked at once a minute.  With the clock at rest nothing looks, so
//! whatever can move them then says so here: an install that finished,
//! a config directory that appeared in the home directory, one of the
//! files written by another program.  Each is counted and knocks on the
//! supervisor; the plugins' next tick reads the count and has their
//! workers look.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use marspot::fs_watch::Watcher;
use marspot_term::lx_warn;

static COUNT: AtomicU64 = AtomicU64::new(0);

/// Something the configuration depends on has changed.
pub(crate) fn note() {
    COUNT.fetch_add(1, Ordering::AcqRel);
    crate::pump::knock();
}

/// How many changes have been noted.
pub(crate) fn count() -> u64 {
    COUNT.load(Ordering::Acquire)
}

/// `.claude`, `.codex-profile-2`, `.kimi-code`: a name an agent's
/// config directory goes by.  Not `.claude.json`, which Claude Code
/// rewrites by the minute.
fn is_config_dir(name: &str) -> bool {
    [".claude", ".codex", ".gemini", ".kimi-code"]
        .iter()
        .any(|base| name.strip_prefix(base).is_some_and(|rest| rest.is_empty() || rest.starts_with("-profile-")))
}

fn config_dirs(home: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(home) else { return Vec::new() };
    let mut names: Vec<String> =
        rd.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|n| is_config_dir(n)).collect();
    names.sort();
    names
}

/// The watches that note a change nobody here made.
pub(crate) struct Watch {
    _home: Option<Watcher>,
    files: Vec<PathBuf>,
    _files: Option<Watcher>,
}

impl Watch {
    /// Watch `home` for config directories coming and going.  Any
    /// other name appearing there is not a change.
    pub(crate) fn of_home(home: PathBuf, on_change: impl Fn() + Send + 'static) -> Watch {
        let mut seen = config_dirs(&home);
        let dir = home.clone();
        let watcher = Watcher::spawn(&[home], move || {
            let now = config_dirs(&dir);
            if now != seen {
                seen = now;
                on_change();
            }
        });
        if let Err(e) = &watcher {
            lx_warn!("agents.config.home_unwatched", &e.to_string());
        }
        Watch { _home: watcher.ok(), files: Vec::new(), _files: None }
    }

    /// Watch `files`, the configuration as the last look found it, in
    /// place of the files watched before.  One whose directory is gone
    /// again is left out: the home watch reports the directory.
    pub(crate) fn follow(&mut self, mut files: Vec<PathBuf>, on_change: impl Fn() + Send + 'static) {
        files.retain(|f| f.parent().is_some_and(Path::is_dir));
        if files == self.files {
            return;
        }
        let watcher = Watcher::spawn(&files, on_change);
        if let Err(e) = &watcher {
            lx_warn!("agents.config.files_unwatched", &e.to_string());
        }
        self._files = watcher.ok();
        self.files = files;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::Duration;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marspot-agent-changes-{tag}-{}", marspot_peer::rand::hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn fired(rx: &Receiver<()>, within: Duration) -> bool {
        rx.recv_timeout(within).is_ok()
    }

    #[test]
    fn config_directories_are_told_from_their_neighbours() {
        for name in [".claude", ".claude-profile-3", ".codex", ".codex-profile-12", ".gemini-profile-2", ".kimi-code", ".kimi-code-profile-1"] {
            assert!(is_config_dir(name), "{name}");
        }
        for name in [".claude.json", ".claude.json.backup", ".codexrc", ".zsh_history", "claude", ".kimi"] {
            assert!(!is_config_dir(name), "{name}");
        }
    }

    /// What an installer does on a machine that never had the agent,
    /// seen with nothing polling.
    #[test]
    fn a_config_directory_appearing_in_home_is_a_change_and_another_file_is_not() {
        let home = scratch("home");
        let (tx, rx) = channel();
        let _w = Watch::of_home(home.clone(), move || tx.send(()).unwrap());
        std::fs::write(home.join(".zsh_history"), "ls\n").unwrap();
        std::fs::write(home.join(".claude.json"), "{}").unwrap();
        assert!(!fired(&rx, Duration::from_millis(300)), "a file that is no config directory");
        std::fs::create_dir(home.join(".claude")).unwrap();
        assert!(fired(&rx, Duration::from_secs(5)), "~/.claude appeared");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_followed_file_written_by_someone_else_is_a_change() {
        let home = scratch("files");
        let settings = home.join("settings.json");
        std::fs::write(&settings, "{}").unwrap();
        let (tx, rx) = channel();
        let mut w = Watch { _home: None, files: Vec::new(), _files: None };
        // one under a directory that does not exist is no reason to watch nothing
        w.follow(vec![settings.clone(), home.join("nowhere").join("config.toml")], move || tx.send(()).unwrap());
        assert_eq!(w.files, std::slice::from_ref(&settings));
        assert!(!fired(&rx, Duration::from_millis(200)), "nothing changed yet");
        // replaced by rename, the way Claude Code saves
        std::fs::write(home.join("settings.tmp"), r#"{"model":"x"}"#).unwrap();
        std::fs::rename(home.join("settings.tmp"), &settings).unwrap();
        assert!(fired(&rx, Duration::from_secs(5)), "the settings file was replaced");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn noting_a_change_moves_the_count() {
        let before = count();
        note();
        assert_eq!(count(), before + 1);
    }
}
