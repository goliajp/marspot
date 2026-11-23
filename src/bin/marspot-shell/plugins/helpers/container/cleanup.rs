//! Taking containers away.  The pane's own script does it when the pane
//! goes; this is for when that did not happen: a helper that is stopped
//! has its container removed once more, and the shell's start removes
//! those whose helper is gone.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::{Shared, docker, pending};

/// A `docker run` hung up before it made its container may make it yet.
const AGAIN_AFTER: Duration = Duration::from_secs(3);

fn remove(docker: &Path, name: &str) -> bool {
    let r = docker::remove(docker, name);
    if let Err(e) = &r {
        marspot::lx_warn!("helpers.container_remove_failed", &format!("{name}: {e}"));
    }
    r.is_ok()
}

fn spawn(what: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name("helpers-container".into()).spawn(f) {
        marspot::lx_warn!("helpers.container_remove_failed", &format!("{what}: {e}"));
    }
}

/// Remove helper `sid`'s container, then its directory, which the
/// container may have written to until then.  One docker would not
/// remove is the next start's to sweep.
pub(in super::super) fn remove_later(sid: u64, root: PathBuf) {
    spawn("stop", move || {
        // read when the container starts, and not after
        let _ = std::fs::remove_file(root.join(sid.to_string()).join("env"));
        let Some(d) = docker::find() else { return };
        let name = docker::container_name(sid);
        remove(&d, &name);
        std::thread::sleep(AGAIN_AFTER);
        if remove(&d, &name) {
            pending::gone(&root, sid);
        } else {
            marspot::lx_warn!("helpers.container_left", &name);
        }
    });
}

/// The containers in `listing` (a name and a session id a line) whose
/// session is not `kept`.
pub(super) fn orphans(listing: &str, kept: impl Fn(u64) -> bool) -> Vec<String> {
    let one = |l: &str| {
        let (name, sid) = l.split_once('\t')?;
        let sid: u64 = sid.trim().parse().ok()?;
        (!kept(sid)).then(|| name.to_string())
    };
    listing.lines().filter_map(one).collect()
}

/// At the shell's start: remove this state directory's containers whose
/// session is gone, and the directories that waited for that.  docker is
/// asked only where there ever was a container.
pub(in super::super) fn sweep(s: &Arc<Shared>, live: HashSet<u64>) {
    if !pending::place(&s.root()).is_dir() {
        return;
    }
    let me = Arc::clone(s);
    spawn("sweep", move || {
        let Some(d) = docker::find() else { return };
        // one started since is in the table before it has a container
        sweep_now(&me.root(), &d, |sid| live.contains(&sid) || me.table().map.contains_key(&sid));
    });
}

pub(super) fn sweep_now(root: &Path, d: &Path, kept: impl Fn(u64) -> bool) {
    sweep_listed(d, &kept);
    for sid in pending::all(root).into_iter().filter(|sid| !kept(*sid)) {
        if remove(d, &docker::container_name(sid)) {
            pending::gone(root, sid);
        }
    }
}

fn sweep_listed(d: &Path, kept: &impl Fn(u64) -> bool) {
    let filter = format!("label={}", docker::state_label());
    let args = ["ps", "-a", "--filter", &filter, "--format", "{{.Names}}\t{{.Label \"marspot.helper\"}}"];
    match docker::run(d, &args, None, docker::ANSWER) {
        Ok(out) if out.ok => {
            for name in orphans(&out.stdout, kept) {
                marspot::lx_info!("helpers.container_orphan", &name);
                remove(d, &name);
            }
        }
        Ok(out) => marspot::lx_warn!("helpers.container_sweep_failed", out.stderr.trim()),
        Err(e) => marspot::lx_warn!("helpers.container_sweep_failed", &e),
    }
}
