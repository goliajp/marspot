//! marspot-shelld — per-user daemon that owns shell PTYs.
//!
//! Run by launchd via `~/Library/LaunchAgents/com.marspot.shelld.plist`
//! (KeepAlive=true).  marspot GUI talks to it over a unix socket at
//! `~/Library/Caches/marspot/shelld.sock`.
//!
//! Why split it out: the GUI process can die and respawn (silent
//! update, crash, manual restart) without taking the user's shell
//! processes with it.  The shell's `getppid()` is launchd, not the
//! GUI; SIGHUP on GUI exit doesn't propagate.  Reattach is a
//! one-frame state replay through `bytelog`.
//!
//! Phase 1 scope: socket bind + accept loop + idle handler.  No
//! protocol yet; just verifies the daemon can be launchd-managed and
//! a client can connect.

use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, IntoRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread;

/// Process-wide shutdown flag.  Set by SIGTERM/SIGINT handler;
/// observed by per-client handlers (the accept loop is woken
/// separately by closing `LISTENER_FD` below — `std`'s accept
/// silently retries EINTR, so a signal alone wouldn't unstick it).
static SHUTDOWN: AtomicBool = AtomicBool::new(false);
/// Raw fd of the listening socket, published after `bind` so the
/// signal handler can `close(2)` it (AS-safe per POSIX) and force
/// the in-flight `accept` to return EBADF.  Negative sentinel before
/// init or after close.
static LISTENER_FD: AtomicI32 = AtomicI32::new(-1);

/// Always-on socket path under `$HOME/Library/Caches/marspot/`.
/// Created on startup, removed on graceful shutdown.  Per-user; no
/// cross-user contention.
fn socket_path() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME unset; refusing to run");
    PathBuf::from(home).join("Library/Caches/marspot/shelld.sock")
}

fn main() {
    eprintln!("[shelld] starting (pid={})", std::process::id());

    let sock = socket_path();
    if let Some(parent) = sock.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("[shelld] failed to create cache dir {}: {}", parent.display(), e);
            std::process::exit(1);
        }
    }
    // Stale socket from a prior crashed instance — `bind` will refuse
    // to attach onto a path that already exists, even if no one is
    // listening.  Safe to nuke unconditionally because launchd
    // guarantees no concurrent shelld (KeepAlive=true, single instance).
    let _ = std::fs::remove_file(&sock);

    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[shelld] bind {} failed: {}", sock.display(), e);
            std::process::exit(1);
        }
    };
    // 0600 — only this user can connect.
    if let Err(e) = std::fs::set_permissions(
        &sock,
        std::fs::Permissions::from_mode(0o600),
    ) {
        eprintln!("[shelld] chmod {} failed: {}", sock.display(), e);
    }
    eprintln!("[shelld] listening on {}", sock.display());

    // Hand the fd's ownership over to us (a raw integer in
    // LISTENER_FD).  std's IO safety machinery would otherwise
    // refuse to share the fd with our signal handler — a process
    // abort fires if std notices the fd was closed externally.  We
    // recreate the UnixListener around the same raw fd inside the
    // loop just to use accept(); that wrapper is `forget`ed each
    // iteration so std doesn't try to drop the fd.
    let raw = listener.into_raw_fd();
    LISTENER_FD.store(raw, Ordering::Release);
    install_signal_handlers();

    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            break;
        }
        // SAFETY: raw is our owned fd; we don't drop the wrapper.
        let wrapper = unsafe { UnixListener::from_raw_fd(raw) };
        let accept_result = wrapper.accept();
        // Don't let std close `raw` when wrapper drops.
        std::mem::forget(wrapper);
        match accept_result {
            Ok((s, _)) => {
                thread::spawn(move || handle_client(s));
            }
            Err(_) => {
                // EBADF from signal handler's close(2) is the
                // intentional wake.  Any other error during normal
                // operation is also fatal for now (we don't have
                // anything productive to do without a listener).
                if SHUTDOWN.load(Ordering::Acquire) {
                    break;
                }
                eprintln!("[shelld] accept error (unexpected)");
                break;
            }
        }
    }
    // Listener may already be closed by signal handler; if not,
    // close it now.  swap returns the previous value atomically.
    let fd = LISTENER_FD.swap(-1, Ordering::AcqRel);
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }

    eprintln!("[shelld] shutting down");
    let _ = std::fs::remove_file(&sock);
}

/// Phase 1 client handler: echoes back whatever the client sends.
/// Replaced wholesale in Phase 2 with the real protocol parser.
fn handle_client(mut stream: UnixStream) {
    let _ = stream.write_all(b"shelld ready\n");
    let mut buf = [0u8; 1024];
    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            break;
        }
        match stream.read(&mut buf) {
            Ok(0) => break, // client closed
            Ok(n) => {
                if stream.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

extern "C" fn signal_handler(sig: libc::c_int) {
    // Async-signal-safe: only an atomic store and a write(2) of a
    // pre-allocated literal.  No allocation, no locks.  The accept
    // loop sees SHUTDOWN on its next iteration (signal delivery
    // interrupts a blocking accept with EINTR, std turns that into
    // io::Error of kind Interrupted, our Err arm checks SHUTDOWN).
    SHUTDOWN.store(true, Ordering::Release);
    let msg: &[u8] = match sig {
        libc::SIGTERM => b"[shelld] SIGTERM\n",
        libc::SIGINT => b"[shelld] SIGINT\n",
        _ => b"[shelld] signal\n",
    };
    unsafe {
        libc::write(libc::STDERR_FILENO, msg.as_ptr() as _, msg.len());
    }
    // std::os::unix::net::UnixListener::accept silently retries on
    // EINTR, so the SHUTDOWN flag alone wouldn't get us out of a
    // blocked accept().  Close the listener fd here — close(2) is
    // on POSIX-1.2008's AS-safe list — to force the in-flight
    // accept to return EBADF; the loop in main then observes
    // SHUTDOWN and exits.
    let fd = LISTENER_FD.swap(-1, Ordering::AcqRel);
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
}

fn install_signal_handlers() {
    // SAFETY: sigaction is the standard POSIX install path; the
    // handler function is `extern "C"` with the right signature, and
    // we only touch a `static` AtomicBool inside it.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = signal_handler as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        // No SA_RESTART — we WANT syscalls to return EINTR so the
        // accept loop can observe SHUTDOWN promptly instead of
        // restarting the accept().
        sa.sa_flags = 0;
        let r1 = libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        let r2 = libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        if r1 != 0 || r2 != 0 {
            eprintln!(
                "[shelld] sigaction failed: TERM={} INT={} errno={}",
                r1,
                r2,
                io::Error::last_os_error()
            );
        }
    }
}
