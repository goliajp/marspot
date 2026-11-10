//! The kimi process behind a pane: which home it runs under and which
//! account profile that is.

use std::path::{Path, PathBuf};

/// The directory kimi keeps everything in: its `KIMI_CODE_HOME`, or
/// `~/.kimi-code`.  The variable names the directory itself, as
/// `CODEX_HOME` does.
pub(super) fn home_from(kimi_code_home: Option<String>, user_home: &Path) -> PathBuf {
    match kimi_code_home {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => default_home(user_home),
    }
}

pub(crate) fn default_home(user_home: &Path) -> PathBuf {
    user_home.join(".kimi-code")
}

/// Account profiles: `~/.kimi-code-profile-N` is a whole kimi home
/// with its own login, settings and sessions, the convention the other
/// agents' profiles follow.
const KIMI_PROFILE_PREFIX: &str = ".kimi-code-profile-";

/// Which profile a home is: `None` for the default one.  Resolved
/// through symlinks, so a home that links to a profile reads as it.
pub(super) fn profile_of(home: &Path) -> Option<u8> {
    let real = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let n: u8 = real.file_name()?.to_str()?.strip_prefix(KIMI_PROFILE_PREFIX)?.parse().ok()?;
    (n > 0).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::handoff::history::tmp;

    #[test]
    fn the_home_is_the_variable_or_the_default_directory() {
        let user = Path::new("/Users/x");
        assert_eq!(home_from(Some("/Users/x/.kimi-code-profile-2".into()), user), Path::new("/Users/x/.kimi-code-profile-2"));
        assert_eq!(home_from(None, user), Path::new("/Users/x/.kimi-code"));
        assert_eq!(home_from(Some(String::new()), user), Path::new("/Users/x/.kimi-code"));
    }

    #[test]
    fn a_numbered_home_is_a_profile_and_the_default_is_not() {
        let home = tmp("kimi-profiles");
        for name in [".kimi-code-profile-2", ".kimi-code-profile-0", ".kimi-code"] {
            std::fs::create_dir_all(home.join(name)).unwrap();
        }
        assert_eq!(profile_of(&home.join(".kimi-code-profile-2")), Some(2));
        assert_eq!(profile_of(&home.join(".kimi-code-profile-0")), None, "0 stands for the default home");
        assert_eq!(profile_of(&home.join(".kimi-code")), None);
        std::os::unix::fs::symlink(home.join(".kimi-code-profile-2"), home.join("linked")).unwrap();
        assert_eq!(profile_of(&home.join("linked")), Some(2), "a link reads as what it points at");
    }
}
