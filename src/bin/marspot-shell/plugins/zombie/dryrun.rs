//! The dry run's own record, `<state>/zombie/dryrun.log`: each suspect
//! with its evidence and each message that would have gone out, whole.
//! The shell log caps a line at a few hundred bytes and folds newlines,
//! so it carries a one-line pointer to each entry and this file the text
//! a person judges the rules by.  0600; past 1 MiB the file moves to
//! `dryrun.log.1`, replacing the one before.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

const ROTATE: u64 = 1 << 20;

/// Append one entry: a header line (`kind`, unix time, then `head`)
/// and `body`, then a blank line.
pub(super) fn append(at: &Path, kind: &str, now: u64, head: &str, body: &str) {
    if let Some(dir) = at.parent() {
        let _ = super::store::ensure(dir);
    }
    if fs::metadata(at).is_ok_and(|m| m.len() > ROTATE) {
        let _ = fs::rename(at, at.with_extension("log.1"));
    }
    let entry = format!("== {kind} {now} {head}\n{body}\n\n");
    let file = OpenOptions::new().create(true).append(true).mode(0o600).open(at);
    if let Err(e) = file.and_then(|mut f| f.write_all(entry.as_bytes())) {
        marspot::lx_warn!("zombie.dryrun.unwritten", &format!("{}: {e}", at.display()));
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn zombie_dryrun_log_appends_whole_entries_and_rotates() {
        let dir = std::env::temp_dir().join(format!("zombie-dryrun-{}", std::process::id()));
        let at = dir.join("zombie").join("dryrun.log");
        append(&at, "suspect", 1, "sid=7 B1 1@2", "line one\nline two");
        let body = fs::read_to_string(&at).unwrap();
        assert_eq!(body, "== suspect 1 sid=7 B1 1@2\nline one\nline two\n\n");
        assert_eq!(fs::metadata(&at).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(at.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        fs::write(&at, vec![b'x'; ROTATE as usize + 1]).unwrap();
        append(&at, "would_tell", 2, "sid=7", "b");
        assert!(at.with_extension("log.1").exists());
        assert_eq!(fs::read_to_string(&at).unwrap(), "== would_tell 2 sid=7\nb\n\n");
        let _ = fs::remove_dir_all(&dir);
    }
}
