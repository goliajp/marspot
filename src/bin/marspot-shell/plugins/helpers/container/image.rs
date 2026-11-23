//! The image helpers run in: built from a file in this build, under a
//! tag that says which content it was built from.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use marspot_peer::control::{ErrorCode, Reject, sha256};

use super::docker;

const DOCKERFILE: &[u8] = include_bytes!("../../../../../../assets/helper-image/Dockerfile");
/// How long a build may take.
const BUILD_WITHIN: Duration = Duration::from_secs(15 * 60);

const REMOVE_WITHIN: Duration = Duration::from_secs(60);

/// One build at a time: helpers started while it runs wait for it.
static BUILDING: Mutex<()> = Mutex::new(());

/// The image's name, of this build's file.
pub(super) fn tag() -> String {
    tag_of(DOCKERFILE)
}

pub(super) fn tag_of(dockerfile: &[u8]) -> String {
    let hash: String = sha256::digest(dockerfile)[..6].iter().map(|b| format!("{b:02x}")).collect();
    format!("marspot-helper:{hash}")
}

/// Say in `dir` which image its script runs: a later build of marspot
/// may have another by the time one that waited for it starts.
pub(super) fn name_in(dir: &Path) -> std::io::Result<()> {
    std::fs::write(dir.join("image"), tag())
}

/// Whether the image is there.
pub(super) fn present(docker: &Path, until: Instant) -> Result<bool, Reject> {
    let none = |m| Reject::new(ErrorCode::NoContainerRuntime, format!("docker did not say whether the image is there: {m}"));
    let o = docker::run(docker, &["image", "inspect", "--format", "{{.Id}}", &tag()], None, docker::left(until)).map_err(none)?;
    Ok(o.ok)
}

/// Remove the images this state directory's marspot built from other
/// contents.  One a container still uses stays: docker refuses.
pub(super) fn remove_older(docker: &Path, keep: &str) {
    let filter = format!("label={}", docker::state_label());
    let args = ["image", "ls", "--filter", "label=marspot.helper.image=1", "--filter", &filter, "--format", "{{.Repository}}:{{.Tag}}"];
    let Ok(out) = docker::run(docker, &args, None, docker::ANSWER) else { return };
    for tag in out.stdout.lines().filter(|t| out.ok && t.starts_with("marspot-helper:") && *t != keep) {
        match docker::run(docker, &["image", "rm", tag], None, REMOVE_WITHIN) {
            Ok(o) if o.ok => marspot::lx_info!("helpers.image_removed", tag),
            Ok(o) => marspot::lx_info!("helpers.image_kept", &format!("{tag}: {}", o.stderr.trim())),
            Err(e) => marspot::lx_warn!("helpers.image_kept", &format!("{tag}: {e}")),
        }
    }
}

/// Have the image, building it if it is not there.
pub(super) fn ready(docker: &Path) -> Result<(), String> {
    let until = Instant::now() + BUILD_WITHIN;
    let _one = BUILDING.lock().unwrap_or_else(|p| p.into_inner());
    let there = || present(docker, Instant::now() + docker::ANSWER).map_err(|r| r.message);
    if there()? {
        return Ok(());
    }
    let left = until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err("the build before it took all of its time".into());
    }
    let tag = tag();
    marspot::lx_info!("helpers.image_build", &tag);
    let out = docker::run(docker, &["build", "--label", "marspot.helper.image=1", "--label", &docker::state_label(), "-t", &tag, "-"], Some(DOCKERFILE), left)?;
    if out.ok {
        remove_older(docker, &tag);
        return Ok(());
    }
    let lines: Vec<&str> = out.stderr.lines().collect();
    Err(lines[lines.len().saturating_sub(12)..].join(" | "))
}
