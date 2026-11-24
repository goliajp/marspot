//! Where Claude Code's session registries are, or would appear.

use std::fs;
use std::path::{Path, PathBuf};

/// The registry directories this user has: `sessions/` under
/// `$CLAUDE_CONFIG_DIR`, `~/.claude` and every `~/.claude-profile-*`,
/// resolved through symlinks so a shared directory appears once.
/// Only directories that exist; none are created.
pub fn default_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in config_dirs().into_iter().filter_map(|c| c.join("sessions").canonicalize().ok()) {
        if dir.is_dir() && !out.contains(&dir) {
            out.push(dir);
        }
    }
    out
}

/// The `sessions/` of each config directory that exists without one
/// yet: where a registry directory would appear.
pub fn missing_dirs() -> Vec<PathBuf> {
    missing_in(config_dirs())
}

pub(super) fn missing_in(configs: Vec<PathBuf>) -> Vec<PathBuf> {
    configs.into_iter().filter(|c| c.is_dir()).map(|c| c.join("sessions")).filter(|d| d.symlink_metadata().is_err()).collect()
}

fn config_dirs() -> Vec<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    config_dirs_of(&home, std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from))
}

/// `$CLAUDE_CONFIG_DIR` (`config`), then those `home` holds.
pub(super) fn config_dirs_of(home: &Path, config: Option<PathBuf>) -> Vec<PathBuf> {
    let mut cands: Vec<PathBuf> = config.into_iter().collect();
    cands.push(home.join(".claude"));
    if let Ok(rd) = fs::read_dir(home) {
        let mut profiles: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with(".claude-profile-")))
            .map(|e| e.path())
            .collect();
        profiles.sort();
        cands.extend(profiles);
    }
    cands
}
