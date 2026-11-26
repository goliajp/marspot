//! The watch's state on disk, `<state>/zombie/` (0700): the pacing in
//! `told.tsv` and `clock.tsv` (0600), so an item told before a shell
//! update is not told again after it and the limits do not start over.
//! The dry run keeps its own pair, `told-dryrun.tsv` and
//! `clock-dryrun.tsv`: what it would have said never keeps a live
//! message from going out, and turning `live` on starts from nothing
//! told.  All are derived state; losing them costs at most a repeated
//! message.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use marspot::zombie::limits::Notify;
use marspot::zombie::tell::Pacer;

pub(super) fn dir() -> PathBuf {
    marspot::paths::state_root().join("zombie")
}

/// Create `dir` and its parents, `dir` itself private to the user.
pub(super) fn ensure(dir: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// The live pacing and the dry run's, each with its own files.
#[derive(Default)]
pub(super) struct Pacers {
    pub live: Pacer,
    pub dry: Pacer,
}

impl Pacers {
    /// The pacing `notify` tells by.
    pub fn of(&mut self, notify: Notify) -> &mut Pacer {
        if notify == Notify::Live { &mut self.live } else { &mut self.dry }
    }

    /// Both, the live one first.
    pub fn both(&mut self) -> [&mut Pacer; 2] {
        [&mut self.live, &mut self.dry]
    }

    /// Both ledgers and clocks as written, to see whether a check changed them.
    pub fn tsv(&self) -> [String; 4] {
        [self.live.told_tsv(), self.live.clock_tsv(), self.dry.told_tsv(), self.dry.clock_tsv()]
    }
}

const LIVE: (&str, &str) = ("told.tsv", "clock.tsv");
const DRY: (&str, &str) = ("told-dryrun.tsv", "clock-dryrun.tsv");

pub(super) fn load(dir: &Path, now: u64) -> Pacers {
    let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap_or_default();
    let one = |(told, clock): (&str, &str)| Pacer::from_tsv(&read(told), &read(clock), now);
    Pacers { live: one(LIVE), dry: one(DRY) }
}

pub(super) fn save(dir: &Path, p: &Pacers) {
    let pair = |(told, clock): (&str, &str), p: &Pacer| write(&dir.join(told), &p.told_tsv()).and_then(|()| write(&dir.join(clock), &p.clock_tsv()));
    let wrote = ensure(dir).and_then(|()| pair(LIVE, &p.live)).and_then(|()| pair(DRY, &p.dry));
    if let Err(e) = wrote {
        marspot::lx_warn!("zombie.state.unwritten", &format!("{}: {e}", dir.display()));
    }
}

/// Replace `at` whole: a reader never sees half a file.
pub(super) fn write(at: &Path, body: &str) -> std::io::Result<()> {
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
        let mut p = Pacers::default();
        p.live.record(7, 1_800_000_000, ["1@2".to_string()]);
        p.dry.record(8, 1_800_000_000, ["3@4".to_string()]);
        save(&dir, &p);
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        for f in ["told.tsv", "clock.tsv", "told-dryrun.tsv", "clock-dryrun.tsv"] {
            assert_eq!(fs::metadata(dir.join(f)).unwrap().permissions().mode() & 0o777, 0o600, "{f}");
        }
        let mut q = load(&dir, 1_800_000_060);
        assert!(q.live.told("1@2") && !q.live.told("3@4"));
        assert!(!q.live.allows(7, 1_800_000_060) && q.live.allows(8, 1_800_000_060));
        let dry = q.of(Notify::DryRun);
        assert!(dry.told("3@4") && !dry.told("1@2"), "the dry run's own ledger");
        assert!(!dry.allows(8, 1_800_000_060));
        let _ = fs::remove_dir_all(&root);
    }
}
