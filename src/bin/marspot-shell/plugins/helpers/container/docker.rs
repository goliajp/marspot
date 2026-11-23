//! The container runtime: the `docker` command line and nothing else.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use marspot_peer::control::{ErrorCode, Reject, sha256};

use crate::bounded::{self, Out};

/// Where it is looked for after PATH: an app opened from Finder has the
/// system's bare one.
const PLACES: [&str; 2] = ["/usr/local/bin", "/opt/homebrew/bin"];
/// How long the daemon may take to answer a question.
pub(super) const ANSWER: Duration = Duration::from_secs(10);

fn path() -> std::ffi::OsString {
    let own = std::env::var_os("PATH").unwrap_or_default();
    let dirs = std::env::split_paths(&own).chain(PLACES.map(PathBuf::from)).collect::<Vec<_>>();
    std::env::join_paths(dirs).unwrap_or(own)
}

/// The docker command, when there is one.
pub(super) fn find() -> Option<PathBuf> {
    std::env::split_paths(&path()).map(|d| d.join("docker")).find(|p| p.is_absolute() && p.is_file())
}

pub(super) fn run(docker: &Path, args: &[&str], input: Option<&'static [u8]>, within: Duration) -> Result<Out, String> {
    // its credential helper and build plugin are looked up on PATH
    bounded::run(Command::new(docker).args(args).env("PATH", path()), input, within)
}

/// How long a question asked now may take.
pub(super) fn left(until: Instant) -> Duration {
    ANSWER.min(until.saturating_duration_since(Instant::now()))
}

const REMOVE: Duration = Duration::from_secs(20);

/// Remove container `name`, which may not be there.
pub(super) fn remove(docker: &Path, name: &str) -> Result<(), String> {
    let o = run(docker, &["rm", "-f", name], None, REMOVE)?;
    if o.ok || o.stderr.contains("No such container") { Ok(()) } else { Err(format!("docker rm: {}", o.stderr.trim())) }
}

/// The docker command, with its daemon running.
pub(super) fn up(until: Instant) -> Result<PathBuf, Reject> {
    let none = |m: String| Reject::new(ErrorCode::NoContainerRuntime, m);
    let docker = find().ok_or_else(|| none("there is no docker command on this machine; use isolation host".into()))?;
    match run(&docker, &["version", "--format", "{{.Server.Version}}"], None, left(until)) {
        Ok(o) if o.ok && !o.stdout.trim().is_empty() => Ok(docker),
        Ok(o) => Err(none(format!("docker's daemon is not running: {}", o.stderr.trim()))),
        Err(e) => Err(none(format!("docker's daemon is not running: {e}"))),
    }
}

fn state_hash() -> String {
    let root = marspot_term::paths::state_root();
    sha256::digest(root.as_os_str().as_encoded_bytes())[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// The container of helper `sid`.  Session ids count per state
/// directory, so the name carries which one it is of: the container of
/// another app on this machine must never be taken for this helper's.
pub(super) fn container_name(sid: u64) -> String {
    format!("marspot-helper-{}-{sid}", state_hash())
}

/// The label that says which state directory a container is of.
pub(super) fn state_label() -> String {
    format!("marspot.state={}", state_hash())
}
