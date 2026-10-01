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

/// FNV-1a over every cell of the published grid.
///
/// Characters only.  Colours and flags ride in the same frame, but a
/// replay that got the text right and the attributes wrong is a different
/// bug from the one this probe is about, and folding both into one hash
/// would report either as the other.
fn grid_fingerprint(session: &Session) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for line in session.screen(COLS, ROWS) {
        for b in line.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h ^= b'\n' as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// One handover: SIGTERM, then wait for the same pid to come back with its
/// history and accept input over a fresh connection.
///
/// `Err` carries what went wrong rather than exiting, so a round number
/// can be put in front of it.
fn handover(session: &Session, pid: u32, id: u64, staged: &std::path::Path) -> Result<(), String> {
    // Every cell, hashed, before the handover.  A marker line surviving
    // says the replay ran; the whole grid matching bit for bit says it
    // replayed the same screen, which is the property
    // `soak-snapshot-survival.sh` was written for and could not check
    // after RFC-003 deleted the daemon it drove.
    let before_fp = grid_fingerprint(session);
    unsafe {
        if libc::kill(pid as i32, libc::SIGTERM) != 0 {
            return Err(format!("SIGTERM {pid}: {}", std::io::Error::last_os_error()));
        }
    }
    // The new image rebinds its readers, so there is a pause and then the
    // same pid answering again.  A pid that is gone means the handler took
    // the quit path instead of the execv one.
    std::thread::sleep(Duration::from_millis(1500));
    if support::rss_kib(pid).is_none() {
        return Err(format!(
            "{pid} is gone after SIGTERM -- the handler took the quit path, which means it \
             did not see a different fingerprint in {}",
            staged.display()
        ));
    }

    // Reattach over the listener, the way L2 does: the inherited control
    // fd died with the old image, and a live pid is not evidence the new
    // one is running -- execv keeps the pid whatever happens next.
    let sock = session_socket_path(id);
    let Some(stream) = support::reattach(&sock, 8) else {
        return Err(format!(
            "{pid} survived but its listener does not accept (connect + Hello handshake) -- \
             L2 would have nothing to reattach to"
        ));
    };

    // The replayed publish first, then compare -- and compare BEFORE
    // typing, since typing changes the grid it is being compared against.
    let replay_deadline = Instant::now() + Duration::from_secs(5);
    let seq_at_signal = session.reader.seq();
    while session.reader.seq() == seq_at_signal && Instant::now() < replay_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let after_fp = grid_fingerprint(session);
    if after_fp != before_fp {
        return Err(format!(
            "the replayed screen differs: grid hash {before_fp:016x} before, \
             {after_fp:016x} after"
        ));
    }

    let (c0, r0) = session.cursor();
    let before = session.reader.seq();
    support::send_char_on(&stream, 's');
    // Wait for a publish from the NEW image.  Reading the region without
    // one proves nothing: shared memory keeps the last frame the old image
    // wrote whether or not anything is alive.
    let deadline = Instant::now() + Duration::from_secs(5);
    while session.reader.seq() == before && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if session.cell(c0, r0, COLS) != 's' {
        return Err(format!("{pid} accepted a reattach but no longer takes input"));
    }
    // Only now is the screen one the new image published.
    if !session
        .screen(COLS, ROWS)
        .iter()
        .any(|l| l.contains(MARKER))
    {
        return Err(format!(
            "{pid} took over and takes input, but the history did not survive: \
             wanted {MARKER:?} in a frame it published"
        ));
    }
    Ok(())
}

fn main() {
    let bin: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| die("usage: l3_swap_probe <marspot-session> [rounds]"));
    let rounds: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    if rounds == 0 {
        die("zero rounds hands nothing over");
    }
    let state = PathBuf::from(
        std::env::var("MARSPOT_STATE_DIR")
            .unwrap_or_else(|_| die("set MARSPOT_STATE_DIR -- this stages a binary under it")),
    );
    let id: u64 = 90_000 + (std::process::id() as u64 % 1000);

    let session = Session::spawn_with_id(&bin, COLS, ROWS, Some(id));
    session.wait_ready("session");
    let pid = session.pid();

    // Something in the history that has to survive every round.
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
    // The replay reads the bytelog from disk, not the grid.
    std::thread::sleep(Duration::from_millis(500));

    // Each round stages a copy whose fingerprint differs from the image
    // that is running NOW -- staging against the original every time would
    // make round two a no-op the handler declines.
    let mut source = bin.clone();
    let mut failure: Option<String> = None;
    for round in 1..=rounds {
        let staged = stage_differing_copy(&source, &state);
        if let Err(e) = handover(&session, pid, id, &staged) {
            failure = Some(format!("round {round} of {rounds}: {e}"));
            break;
        }
        // Next round differs from what is now running, which is `staged`.
        source = staged;
    }

    let mut all = vec![session];
    cleanup(&mut all);
    let _ = std::fs::remove_file(state.join("binaries/current/marspot-session"));

    if let Some(e) = failure {
        die(e);
    }
    println!(
        "PASS: session {id} handed over {rounds}x in place -- same pid {pid} throughout, \
         {MARKER:?} still on screen, echoes"
    );
}
