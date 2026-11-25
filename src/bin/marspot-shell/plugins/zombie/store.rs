//! The watch's state on disk, `<state>/zombie/` (0700): the pacing in
//! `told.tsv` and `clock.tsv` (0600), so an item told before a shell
//! update is not told again after it and the limits do not start over.
//! Both are derived state; losing them costs at most a repeated message.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use marspot::zombie::tell::Pacer;

pub(super) fn dir() -> PathBuf {
    marspot::paths::state_root().join("zombie")
}

/// Create `dir` and its parents, `dir` itself private to the user.
pub(super) fn ensure(dir: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

pub(super) fn load(dir: &Path, now: u64) -> Pacer {
    let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap_or_default();
    Pacer::from_tsv(&read("told.tsv"), &read("clock.tsv"), now)
}

pub(super) fn save(dir: &Path, p: &Pacer) {
    let wrote = ensure(dir).and_then(|()| write(&dir.join("told.tsv"), &p.told_tsv())).and_then(|()| write(&dir.join("clock.tsv"), &p.clock_tsv()));
    if let Err(e) = wrote {
        marspot::lx_warn!("zombie.state.unwritten", &format!("{}: {e}", dir.display()));
    }
}

/// Replace `at` whole: a reader never sees half a file.
fn write(at: &Path, body: &str) -> std::io::Result<()> {
    let tmp = at.with_extension("tsv.new");
    let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(body.as_bytes())?;
    fs::rename(&tmp, at)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn zombie_state_is_private_and_read_back_by_a_new_checker() {
        let root = std::env::temp_dir().join(format!("zombie-store-{}", std::process::id()));
        let dir = root.join("zombie");
        let mut p = Pacer::default();
        p.record(7, 1_800_000_000, ["1@2".to_string()]);
        save(&dir, &p);
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        for f in ["told.tsv", "clock.tsv"] {
            assert_eq!(fs::metadata(dir.join(f)).unwrap().permissions().mode() & 0o777, 0o600, "{f}");
        }
        let q = load(&dir, 1_800_000_060);
        assert!(q.told("1@2"));
        assert!(!q.allows(7, 1_800_000_060));
        let _ = fs::remove_dir_all(&root);
    }
}
