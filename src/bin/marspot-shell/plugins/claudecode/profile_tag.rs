//! Which account a pane's claude is on, from the config dir it was
//! started with.

use std::path::PathBuf;

use super::idle::PROFILE_UNKNOWN;

/// The profile number of a claude started with no `CLAUDE_CONFIG_DIR`:
/// the CLI's own default directory, which no numbered profile is.
pub(super) const PROFILE_DEFAULT: u8 = 0;

/// The badge tag and profile number for a pane.
///
/// Takes the directory rather than the pid because everything a pane
/// says about its profile — the tag, where its transcripts live, the
/// config dir a resume line has to set — has to come from **one
/// read**.  Three separate reads is how the tag came to say `P3`
/// while the transcript walk looked under the default profile and
/// found nothing (2026-08-10).
///
/// `env_read` says the process's environment was readable.  With no
/// config dir in it the pane is on the default account: no tag, since
/// a machine with one account has nothing to tell apart, and a number
/// that lets it be parked and brought back like any other.  With the
/// environment unreadable nothing is known, and the pane is left
/// alone.
pub(super) fn tag_and_profile(config_dir: Option<&str>, env_read: bool) -> (Option<String>, u8) {
    let Some(dir) = config_dir else {
        return (None, if env_read { PROFILE_DEFAULT } else { PROFILE_UNKNOWN });
    };
    let base = std::path::Path::new(dir).file_name().map(|b| b.to_string_lossy().into_owned());
    match base.as_deref() {
        Some(".claude") => (Some("P0".into()), PROFILE_DEFAULT),
        Some(b) => match b.strip_prefix(".claude-profile-").and_then(|n| n.parse::<u8>().ok()) {
            Some(n) => (Some(format!("P{n}")), n),
            // an unrecognised directory: say so, rather than show no
            // tag, which reads as the default account
            None => (Some("P?".into()), PROFILE_UNKNOWN),
        },
        None => (None, PROFILE_UNKNOWN),
    }
}

/// Where this pane keeps its transcripts, from the same one read.
pub(super) fn projects_root_from(config_dir: Option<&str>) -> Option<PathBuf> {
    let dir = config_dir?.trim();
    if dir.is_empty() {
        return None;
    }
    Some(PathBuf::from(dir).join("projects"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pane_is_tagged_by_the_directory_it_was_started_with() {
        assert_eq!(tag_and_profile(Some("/Users/x/.claude-profile-3"), true), (Some("P3".into()), 3));
        assert_eq!(tag_and_profile(Some("/Users/x/.claude-profile-3/"), true), (Some("P3".into()), 3));
        assert_eq!(tag_and_profile(Some("/Users/x/.claude"), true), (Some("P0".into()), PROFILE_DEFAULT));
        assert_eq!(tag_and_profile(Some("/somewhere/else"), true), (Some("P?".into()), PROFILE_UNKNOWN));
    }

    /// One account, started as plain `claude`: known, and untagged.
    #[test]
    fn no_config_dir_is_the_default_account_only_when_the_environment_was_read() {
        assert_eq!(tag_and_profile(None, true), (None, PROFILE_DEFAULT));
        assert_eq!(tag_and_profile(None, false), (None, PROFILE_UNKNOWN));
    }
}
