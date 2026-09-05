//! RFC-007 — start L3 through `launchd` so the user's shell chain is
//! free of `com.apple.provenance`.
//!
//! Every process under a user-installed `.app` carries that mark, it is
//! inherited by children AND by whoever execs a marked file, and every
//! file such a process writes is marked too.  A binary the user builds
//! inside marspot is therefore marked, and its first execution pays a
//! full Gatekeeper scan — a notarisation round trip (3 s timeout, with
//! retries) plus an XProtect pass, serialised through one `syspolicyd`.
//! Measured here, same command, same minute: 0.025–0.16 s on a clean
//! chain against 0.34–4.1 s on marspot's.  Reports from a heavier test
//! tier reached 30 s and 268 s.
//!
//! Two things are required and neither is sufficient alone: the process
//! must not descend from the app, and the file it execs must itself be
//! unmarked.  So this module keeps an unmarked copy of the L3 binary
//! (written BY a clean process, since the attribute cannot be removed
//! afterwards) and boots L3 from it as a `launchd` job.
//!
//! `MARSPOT_CLEAN_EXEC=0` returns to the direct `Command::spawn` path.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use marspot_term::paths;

/// Where the unmarked copies live, beside `current/`.
pub fn clean_bin_dir() -> PathBuf {
    paths::binaries_root().join("clean")
}

/// Label for a session's job.  The state root is folded in so a dev
/// sandbox and the installed app never collide in `launchd`'s
/// namespace — they are different marspots with the same session ids.
pub fn session_job_label(session_id: u64) -> String {
    let root = paths::state_root();
    let mut h: u64 = 0xcbf29ce484222325;
    for b in root.as_os_str().as_encoded_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("jp.golia.marspot.session.{h:x}.{session_id}")
}

fn gui_domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

/// Single-quote for `/bin/sh`.  Paths here come from our own state
/// root, but a home directory can contain anything.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn plist_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Ask `launchd` to run `argv` and wait for the job to finish.
///
/// Used for the copy step, where the whole point is that the writing
/// process is not a descendant of this one.
fn run_via_launchd_sync(label: &str, argv: &[&str], timeout: std::time::Duration) -> io::Result<()> {
    let dir = paths::state_root().join("launchd");
    std::fs::create_dir_all(&dir)?;
    let plist_path = dir.join(format!("{label}.plist"));
    let mut args = String::new();
    for a in argv {
        args.push_str(&format!("<string>{}</string>", plist_escape(a)));
    }
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{label}</string>
<key>ProgramArguments</key><array>{args}</array>
<key>RunAtLoad</key><true/>
<key>AbandonProcessGroup</key><true/>
</dict></plist>"#
    );
    std::fs::write(&plist_path, plist)?;
    let domain = gui_domain();
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("{domain}/{label}")])
        .output();
    let out = Command::new("/bin/launchctl")
        .args(["bootstrap", &domain, &plist_path.to_string_lossy()])
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    // Wait for the job to leave the domain's running set.
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let p = Command::new("/bin/launchctl")
            .args(["print", &format!("{domain}/{label}")])
            .output()?;
        let txt = String::from_utf8_lossy(&p.stdout);
        if !p.status.success() || txt.contains("state = not running") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("{domain}/{label}")])
        .output();
    let _ = std::fs::remove_file(&plist_path);
    Ok(())
}

/// An unmarked copy of `src`, refreshed when `src` changes.
///
/// The copy is made by `/bin/cp` under `launchd`, so the writing
/// process carries no mark and neither does what it writes.  Freshness
/// is (len, mtime) of the source recorded beside the copy — a cdhash
/// would be stronger but costs a `codesign` exec on a path that runs
/// whenever a pane opens, and any change to the source moves mtime.
pub fn ensure_clean_copy(src: &Path) -> io::Result<PathBuf> {
    let dir = clean_bin_dir();
    std::fs::create_dir_all(&dir)?;
    let name = src
        .file_name()
        .ok_or_else(|| io::Error::other("source has no file name"))?;
    let dst = dir.join(name);
    let stamp_path = dir.join(format!("{}.stamp", name.to_string_lossy()));

    let meta = std::fs::metadata(src)?;
    let want = format!(
        "{}:{}",
        meta.len(),
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    if dst.exists() && std::fs::read_to_string(&stamp_path).ok().as_deref() == Some(want.as_str()) {
        return Ok(dst);
    }

    // `cp` copies extended attributes on macOS, which would carry the
    // very mark this copy exists to shed (measured: the first version
    // of this produced a stamped copy).  Redirect through `cat` so the
    // destination is a genuinely new file, then rename it into place —
    // a fresh inode, which the signature cache also requires.
    let tmp = dir.join(format!("{}.new", name.to_string_lossy()));
    let script = format!(
        "cat {src} > {tmp} && chmod 755 {tmp} && mv -f {tmp} {dst}",
        src = shell_quote(&src.to_string_lossy()),
        tmp = shell_quote(&tmp.to_string_lossy()),
        dst = shell_quote(&dst.to_string_lossy()),
    );
    let label = format!("{}.copy", session_job_label(0));
    run_via_launchd_sync(
        &label,
        &["/bin/sh", "-c", &script],
        std::time::Duration::from_secs(30),
    )?;
    if !dst.exists() {
        return Err(io::Error::other("clean copy did not appear"));
    }
    std::fs::write(&stamp_path, &want)?;
    Ok(dst)
}

/// Boot an L3 as a `launchd` job.  Returns once `launchctl` has
/// accepted it; the caller then waits for the session's socket exactly
/// as it does for a forked child.
///
/// `AbandonProcessGroup` keeps a later `bootout` from reaching into the
/// shell's process group, and the job carries no `KeepAlive`: when L3
/// exits it stays exited, which is what retirement means here.
pub fn boot_session_job(
    label: &str,
    bin: &Path,
    env: &[(String, String)],
) -> io::Result<()> {
    let dir = paths::state_root().join("launchd");
    std::fs::create_dir_all(&dir)?;
    let plist_path = dir.join(format!("{label}.plist"));
    let mut envxml = String::new();
    for (k, v) in env {
        envxml.push_str(&format!(
            "<key>{}</key><string>{}</string>",
            plist_escape(k),
            plist_escape(v)
        ));
    }
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{label}</string>
<key>ProgramArguments</key><array><string>{bin}</string></array>
<key>EnvironmentVariables</key><dict>{envxml}</dict>
<key>RunAtLoad</key><true/>
<key>AbandonProcessGroup</key><true/>
<key>ProcessType</key><string>Interactive</string>
</dict></plist>"#,
        bin = plist_escape(&bin.to_string_lossy())
    );
    std::fs::write(&plist_path, plist)?;
    let domain = gui_domain();
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("{domain}/{label}")])
        .output();
    let out = Command::new("/bin/launchctl")
        .args(["bootstrap", &domain, &plist_path.to_string_lossy()])
        .output()?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&plist_path);
        return Err(io::Error::other(format!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Remove a session's job.  Called when the session retires; safe to
/// call for a job that is already gone.
pub fn bootout_session_job(session_id: u64) {
    let label = session_job_label(session_id);
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("{}/{}", gui_domain(), label)])
        .output();
    let _ = std::fs::remove_file(paths::state_root().join("launchd").join(format!("{label}.plist")));
}

/// Is the clean-exec path enabled?  On by default; `MARSPOT_CLEAN_EXEC=0`
/// returns to the direct fork, which is what a bisect wants.
pub fn enabled() -> bool {
    std::env::var("MARSPOT_CLEAN_EXEC").map(|v| v != "0").unwrap_or(true)
}

/// Run a `/bin/sh -c` script through a `launchd` job and wait for it.
/// Used by `examples/clean_exec_probe` to demonstrate the property this
/// module exists for, without standing up a whole L3.
pub fn run_probe_script(script: &str) -> io::Result<()> {
    let label = format!("{}.probe", session_job_label(0));
    run_via_launchd_sync(
        &label,
        &["/bin/sh", "-c", script],
        std::time::Duration::from_secs(15),
    )
}
