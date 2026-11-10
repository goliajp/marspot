//! The model a pane's gemini session was set to when the pane left it.
//!
//! gemini does not keep the setting with the session: a resumed one
//! takes whatever the flag, the environment and the settings say at
//! that moment.  A pane that comes back to its gemini session would
//! otherwise come back on a different model without anyone asking for
//! that, so the setting is written down on the way out and put on the
//! command line on the way back.  One small file per pane, removed with
//! the pane's ledger.

use std::path::{Path, PathBuf};

use super::ledger;
use crate::plugins::gemini;

fn path(dir: &Path, sid: u64) -> PathBuf {
    dir.join(format!("{sid}.model"))
}

fn remember_in(dir: &Path, sid: u64, session: &str, model: Option<&str>) -> std::io::Result<()> {
    let file = path(dir, sid);
    match model {
        Some(model) => {
            std::fs::create_dir_all(dir)?;
            std::fs::write(file, format!("{session} {model}\n"))
        }
        // not known this time: what was written for an earlier leave
        // must not stand in for it
        None => match std::fs::remove_file(file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
}

/// Note what pane `sid`'s gemini `session` is set to as the pane leaves
/// it; `None` when that is not known.
pub fn remember_gemini_model(sid: u64, session: &str, model: Option<&str>) -> std::io::Result<()> {
    remember_in(&ledger::dir(), sid, session, model)
}

/// What `session` was set to when pane `sid` left it.  Only for that
/// session: one the pane has moved on from says nothing about another.
pub(super) fn gemini_model(dir: &Path, sid: u64, session: &str) -> Option<String> {
    let text = std::fs::read_to_string(path(dir, sid)).ok()?;
    let (of, model) = text.strip_suffix('\n')?.split_once(' ')?;
    (of == session && gemini::is_model(model)).then(|| model.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::handoff::history::tmp;

    #[test]
    fn the_model_is_kept_for_the_session_it_was_set_on() {
        let d = tmp("carried");
        assert_eq!(gemini_model(&d, 4, "s-1"), None, "never left");
        remember_in(&d, 4, "s-1", Some("gemini-2.5-pro")).unwrap();
        assert_eq!(gemini_model(&d, 4, "s-1").as_deref(), Some("gemini-2.5-pro"));
        assert_eq!(gemini_model(&d, 4, "s-2"), None, "another session");
        assert_eq!(gemini_model(&d, 5, "s-1"), None, "another pane");
        remember_in(&d, 4, "s-1", None).unwrap();
        assert_eq!(gemini_model(&d, 4, "s-1"), None, "left again with the model not known");
        remember_in(&d, 4, "s-1", None).unwrap();
    }

    #[test]
    fn a_file_that_is_not_one_of_these_names_no_model() {
        let d = tmp("carried-bad");
        for text in ["", "s-1 pro", "s-1\n", "s-1 two words\n", "s-1 x;rm\n", "s-1 -m\n"] {
            std::fs::write(path(&d, 4), text).unwrap();
            assert_eq!(gemini_model(&d, 4, "s-1"), None, "{text:?}");
        }
    }

    #[test]
    fn the_file_goes_when_the_pane_does() {
        let d = tmp("carried-prune");
        remember_in(&d, 4, "s-1", Some("pro")).unwrap();
        remember_in(&d, 5, "s-2", Some("pro")).unwrap();
        assert_eq!(ledger::prune(&d, &[5]), 1);
        assert_eq!(gemini_model(&d, 4, "s-1"), None);
        assert_eq!(gemini_model(&d, 5, "s-2").as_deref(), Some("pro"));
    }
}
