//! Having a staged core compile its shaders before it is promoted.
//!
//! The system keeps compiled shaders per binary.  A core that has never
//! run compiles them from source at its start (70 ms measured), and at
//! an update that start is the stretch where no core is drawing.  The
//! staged binary is run once with `--warm-shaders` while the old core
//! is still serving, so the swap finds them compiled.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use marspot::{lx_event, lx_warn};

/// A compile is tens of milliseconds; past this the system is not
/// answering, and the update goes on without the warm-up.
const WARM_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether the staged core starts, and, when it does, its shaders
/// compiled ahead of the swap.  The warm-up cannot fail an update: a
/// core that starts is promoted whether or not it warmed.
pub(crate) fn can_start_warmed(bin: &Path) -> bool {
    let starts = marspot::binary_tree::can_start(bin);
    if starts && version_of(bin).is_some_and(|v| v >= FIRST_THAT_WARMS) {
        warm(bin, WARM_TIMEOUT);
    }
    starts
}

/// The first core that answers `--warm-shaders`.  An older one takes
/// the argument for none and starts up as a core would: it opens the
/// log, finds no surface to draw to and panics into it.  A staged core
/// can be older than this shell (a downgrade), so it is asked for its
/// version first.
const FIRST_THAT_WARMS: [u32; 3] = [0, 12, 512];

/// What `bin --version` prints, as numbers.  Asked of a binary that has
/// just answered the same question for the start probe.
fn version_of(bin: &Path) -> Option<[u32; 3]> {
    let out = Command::new(bin)
        .arg("--version")
        .env("MARSPOT_NO_REDIRECT", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

fn parse_version(printed: &str) -> Option<[u32; 3]> {
    let mut parts = printed.trim().split('.').map(|p| p.parse::<u32>().ok());
    let v = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(v)
}

/// Run `bin --warm-shaders` to its end, or kill it at `timeout`.  Runs
/// on the probe's thread; leaves no process behind either way.
fn warm(bin: &Path, timeout: Duration) {
    let t0 = Instant::now();
    let spawned = Command::new(bin)
        .arg("--warm-shaders")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            lx_warn!("update.shader_warm.spawn_failed", &format!("{}: {e}", bin.display()));
            return;
        }
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if t0.elapsed() < timeout => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                // not yet reaped, so the group id is still this child's
                // SAFETY: a signal to a process group this thread made
                unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
                let _ = child.wait();
                break None;
            }
        }
    };
    let mut printed = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut out, &mut printed);
    }
    match status {
        Some(st) if st.success() => lx_event!(
            "UPDATE_SHADER_WARM",
            "the staged core compiled its shaders ahead of the swap",
            took_ms = t0.elapsed().as_millis() as u64,
            library_pipelines_us = printed.trim()
        ),
        Some(st) => lx_warn!("update.shader_warm.failed", &format!("{} answered {st}", bin.display())),
        None => lx_warn!("update.shader_warm.timed_out", &format!("{} was stopped after {timeout:?}", bin.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(name: &str, body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("marspot-warm-{}-{name}", std::process::id()));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A candidate that never answers is stopped at the bound, with
    /// whatever it started, and nothing is left running.
    #[test]
    fn a_warm_up_that_hangs_is_stopped_with_its_children() {
        let marker = std::env::temp_dir().join(format!("marspot-warm-{}-pid", std::process::id()));
        let bin = script("hang", &format!("sleep 600 &\necho $! > {}\nwait", marker.display()));
        let t0 = Instant::now();
        warm(&bin, Duration::from_millis(300));
        assert!(t0.elapsed() < Duration::from_secs(5), "the bound did not hold: {:?}", t0.elapsed());
        let grandchild: i32 = std::fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
        // the kill is delivered at once; the process table catches up
        let mut gone = false;
        for _ in 0..200 {
            // SAFETY: signal 0 only asks whether the process exists
            if unsafe { libc::kill(grandchild, 0) } != 0 {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_file(&bin);
        let _ = std::fs::remove_file(&marker);
        assert!(gone, "the candidate's own child {grandchild} is still running");
    }

    /// A candidate that fails its warm-up is still one that starts.
    #[test]
    fn a_failed_warm_up_does_not_fail_the_candidate() {
        let marker = std::env::temp_dir().join(format!("marspot-warm-{}-asked", std::process::id()));
        let bin = script("fail", &format!("[ \"$1\" = --version ] && echo 0.12.99999 && exit 0\n: > {}\nexit 3", marker.display()));
        let ok = can_start_warmed(&bin);
        let asked = std::fs::remove_file(&marker).is_ok();
        let _ = std::fs::remove_file(&bin);
        assert!(asked, "the candidate was never asked to warm up");
        assert!(ok);
    }

    /// A core from before the warm-up would start up for real on the
    /// argument: it is not run with it.
    #[test]
    fn a_core_from_before_the_warm_up_is_not_asked() {
        let marker = std::env::temp_dir().join(format!("marspot-warm-{}-old-asked", std::process::id()));
        let bin = script("old", &format!("[ \"$1\" = --version ] && echo 0.12.491 && exit 0\n: > {}", marker.display()));
        let ok = can_start_warmed(&bin);
        let asked = std::fs::remove_file(&marker).is_ok();
        let _ = std::fs::remove_file(&bin);
        assert!(ok);
        assert!(!asked, "an older core was run with an argument it does not know");
    }

    #[test]
    fn a_version_is_three_numbers() {
        assert_eq!(parse_version("0.12.510\n"), Some([0, 12, 510]));
        assert!([0, 12, 512] >= FIRST_THAT_WARMS && [0, 12, 511] < FIRST_THAT_WARMS && [0, 13, 0] > FIRST_THAT_WARMS);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("0.12"), None);
        assert_eq!(parse_version("0.12.x"), None);
        assert_eq!(parse_version("0.12.5.1"), None);
    }
}
