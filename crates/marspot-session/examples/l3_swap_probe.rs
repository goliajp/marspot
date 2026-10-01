//! A pane's process replaced under it, without the pane noticing.
//!
//! This is the load-bearing mechanism of silent update. The user's
//! scrollback, half-typed command and running program all have to survive
//! a new build taking over, so the handover is not a new process: on
//! SIGTERM an L3 compares its own rodata fingerprint with the one in
//! `current/marspot-session`, and if they differ it execv's itself.
//! The pid, the PTY master fd, the UDS listener and the shell child are
//! all the same afterwards. What does not survive is the control socket
//! L2 passed in as an inherited fd: the new image rebinds its readers and
//! L2 reconnects over `sessions/<id>/sock`, which is what the listener is
//! for. A probe that keeps writing to the old fd gets a broken pipe and
//! reads it as the session dying.
//!
//! The fingerprint comparison is the whole switch, so the probe stages a
//! copy of the same binary with one byte of its `MARSPOT_FP` marker
//! changed. It only has to differ; it does not have to be a real build --
//! but it does have to be loadable. Patching a byte invalidates the code
//! signature ("code or signature have been modified"), macOS refuses to
//! exec it, and the first version of this probe read that as the product
//! failing to hand over. The copy is re-signed adhoc and the signature is
//! verified before anything is signalled.
//! A matching fingerprint means no update, and the handler then takes the
//! user-quit path instead -- state.bin, clean exit, SIGHUP to the shell --
//! which is why `begin_pane_swap` promotes before it signals and why this
//! probe would otherwise be testing pane closure.
//!
//! Written 2026-10-01. What this file used to hold tested
//! `begin_pane_swap`'s old mechanism -- a replacement L3 on the same
//! session id -- which cannot work: `sessions/<id>/` is locked by the live
//! L3 and a second one refuses to start.
//!
//!     MARSPOT_STATE_DIR=/tmp/marspot-dev \
//!       cargo run -p marspot-session --example l3_swap_probe -- \
//!       target/release/marspot-session

#[path = "support/mod.rs"]
mod support;

use marspot_term::session_registry::session_socket_path;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use support::{COLS, ROWS, Session, cleanup, die};

const MARKER: &str = "swapmark";

/// Copy `from` to `<state>/binaries/current/marspot-session` with one
/// byte of its fingerprint changed, so the running L3 sees a different
/// image next door.
fn stage_differing_copy(from: &std::path::Path, state: &std::path::Path) -> PathBuf {
    let dir = state.join("binaries/current");
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| die(format!("mkdir {}: {e}", dir.display())));
    let dst = dir.join("marspot-session");
    let mut bytes =
        std::fs::read(from).unwrap_or_else(|e| die(format!("read {}: {e}", from.display())));
    let needle = b"MARSPOT_FP=";
    let at = bytes
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap_or_else(|| die("no MARSPOT_FP marker in the binary to differ from"));
    // First character of the sha that follows the marker.
    let i = at + needle.len();
    bytes[i] = if bytes[i] == b'0' { b'1' } else { b'0' };
    let _ = std::fs::remove_file(&dst);
    std::fs::write(&dst, &bytes).unwrap_or_else(|e| die(format!("write {}: {e}", dst.display())));
    let mut perms = std::fs::metadata(&dst).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
    }
    std::fs::set_permissions(&dst, perms).ok();
    // Quarantine xattrs would make the exec fail for a reason that has
    // nothing to do with the handover.
    let _ = std::process::Command::new("xattr").arg("-c").arg(&dst).status();
    // Re-sign: the patch above invalidated the signature, and macOS will
    // not exec a binary whose signature does not match its contents.
    let signed = std::process::Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(&dst)
        .status();
    if !signed.map(|s| s.success()).unwrap_or(false) {
        die("could not re-sign the staged copy");
    }
    // And say so rather than discovering it as a failed handover.
    let ok = std::process::Command::new("codesign")
        .arg("-v")
        .arg(&dst)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        die(format!("the staged copy at {} does not verify", dst.display()));
    }
    dst
}

fn main() {
    let bin: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| die("usage: l3_swap_probe <marspot-session>"));
    let state = PathBuf::from(
        std::env::var("MARSPOT_STATE_DIR")
            .unwrap_or_else(|_| die("set MARSPOT_STATE_DIR -- this stages a binary under it")),
    );
    let id: u64 = 90_000 + (std::process::id() as u64 % 1000);

    let session = Session::spawn_with_id(&bin, COLS, ROWS, Some(id));
    session.wait_ready("session");
    let pid = session.pid();

    // Something in the history that has to still be there afterwards.
    session.send_line(&format!("printf '%s\\n' {MARKER}"));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if session
            .screen(COLS, ROWS)
            .iter()
            .any(|l| l.trim_end() == MARKER)
        {
            break;
        }
        if Instant::now() >= deadline {
            let mut all = vec![session];
            cleanup(&mut all);
            die(format!("{MARKER:?} never reached the screen"));
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    std::thread::sleep(Duration::from_millis(400));
    let staged = stage_differing_copy(&bin, &state);

    // The trigger L2 uses, and all it uses.
    unsafe {
        if libc::kill(pid as i32, libc::SIGTERM) != 0 {
            die(format!(
                "SIGTERM {pid}: {}",
                std::io::Error::last_os_error()
            ));
        }
    }

    // The new image rebinds its readers, so there is a pause and then the
    // same pid answering again.  A pid that is gone means the handler took
    // the quit path instead of the execv one.
    std::thread::sleep(Duration::from_millis(1500));
    let alive = support::rss_kib(pid).is_some();

    // Reattach over the listener, the way L2 does: the inherited control
    // fd died with the old image.  A successful connect is the first
    // evidence that the NEW image is running -- a live pid is not, since
    // execv keeps the pid whatever happens next.
    let mut reattached = false;
    let mut echoed = false;
    let mut carried = false;
    if alive {
        let sock = session_socket_path(id);
        let stream = support::reattach(&sock, 8);
        reattached = stream.is_some();
        if let Some(stream) = stream {
            let (c0, r0) = session.cursor();
            let before = session.reader.seq();
            support::send_char_on(&stream, 's');
            // Wait for a publish from the new image.  Reading the region
            // without one proves nothing: shared memory keeps the last
            // frame the OLD image wrote whether or not anyone is alive,
            // so "the history is still there" was a free pass.
            let deadline = Instant::now() + Duration::from_secs(5);
            while session.reader.seq() == before && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            echoed = session.cell(c0, r0, COLS) == 's';
            // Only now is the screen one the new image published.
            carried = session.screen(COLS, ROWS).iter().any(|l| l.contains(MARKER));
        }
    }

    let mut all = vec![session];
    cleanup(&mut all);
    let _ = std::fs::remove_file(&staged);

    if !alive {
        die(format!(
            "{pid} is gone after SIGTERM -- the handler took the quit path, which means it \
             did not see a different fingerprint in {}",
            staged.display()
        ));
    }

    if !reattached {
        die(format!(
            "{pid} survived with its history but its listener does not accept -- \
             L2 would have nothing to reattach to (connect + Hello handshake)"
        ));
    }
    if !echoed {
        die(format!("{pid} accepted a reattach but no longer takes input"));
    }
    if !carried {
        die(format!(
            "{pid} took over and takes input, but the history did not survive the \
             handover: wanted {MARKER:?} in a frame it published"
        ));
    }
    println!("PASS: {pid} execv'd itself in place -- same pid, {MARKER:?} still on screen, echoes");
}
