//! L1's `fd-vault` server — RFC-003 §6 Amendment 15.
//!
//! Spins up a UDS listener at `paths::state_root()/l1-fd-vault.sock`
//! and serves the `marspot_term::fd_vault` protocol.  L1 owns this
//! thread (one dedicated accept thread + one short-lived thread per
//! connection); the vault state is a single `Mutex<HashMap<u64,
//! (OwnedFd, Vec<u8>)>>` shared via `Arc`.
//!
//! L1 doesn't know or care what's in the keys / metadata / fds — it
//! just buffers them on behalf of whoever wants to store + retrieve.
//! See `crates/marspot-term/src/fd_vault.rs` doc-comment for the
//! design rationale (decoupling stable layer from L2's business).

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use marspot_term::fd_vault::{
    self, decode_deposit, decode_withdraw, encode_delivered, recv_frame_with_fd,
    send_frame_no_fd, send_frame_with_fd, server_handshake, vault_socket_path, VaultMsg,
};
use marspot_term::{lx_error, lx_event, lx_info, lx_warn};

/// One vault entry: the kernel-object fd we're holding for someone +
/// the opaque metadata blob they handed in with it.
type Entry = (OwnedFd, Vec<u8>);

#[derive(Clone)]
pub struct VaultServer {
    inner: Arc<Mutex<HashMap<u64, Entry>>>,
    sock_path: PathBuf,
}

impl VaultServer {
    /// Bind the listener, write a fresh sock file (unlinking any stale
    /// one from a previous L1), spawn the accept thread.  Returns a
    /// handle the caller can clone into other places (e.g. close
    /// cleanup).  Failure to bind is fatal — the rest of L1 depends
    /// on the vault being up.
    pub fn start() -> io::Result<Self> {
        let sock_path = vault_socket_path();
        if let Some(parent) = sock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Stale sock from a crashed prior L1: best-effort unlink.
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path)?;
        let inner: Arc<Mutex<HashMap<u64, Entry>>> = Arc::new(Mutex::new(HashMap::new()));

        let inner_thread = Arc::clone(&inner);
        let path_for_log = sock_path.clone();
        thread::Builder::new()
            .name("l1-fd-vault-accept".to_string())
            .spawn(move || accept_loop(listener, inner_thread, path_for_log))?;

        lx_event!(
            "L1_VAULT_UP",
            "fd-vault server bound + accept thread started",
            sock = sock_path.display()
        );

        Ok(Self {
            inner,
            sock_path,
        })
    }

    /// Number of fds currently in the vault.  Diagnostic only.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// Drop every entry — closes the OwnedFds, which decrements the
    /// kernel-object refcount.  Called by L1's close path so the
    /// shells whose PTYs we were holding get SIGHUP'd along with
    /// everything else (user-driven quit = clean account, per
    /// `project_marspot_is_one_product`).
    pub fn drain(&self) -> usize {
        self.inner
            .lock()
            .map(|mut g| {
                let n = g.len();
                g.clear();
                n
            })
            .unwrap_or(0)
    }

    pub fn sock_path(&self) -> &std::path::Path {
        &self.sock_path
    }
}

fn accept_loop(
    listener: UnixListener,
    state: Arc<Mutex<HashMap<u64, Entry>>>,
    sock_path: PathBuf,
) {
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let state_clone = Arc::clone(&state);
                if let Err(e) = thread::Builder::new()
                    .name("l1-fd-vault-conn".to_string())
                    .spawn(move || {
                        if let Err(e) = handle_conn(stream, state_clone) {
                            lx_warn!("l1.vault.conn_failed", &format!("{e}"));
                        }
                    })
                {
                    lx_error!("l1.vault.spawn_failed", &format!("{e}"));
                }
            }
            Err(e) => {
                lx_info!(
                    "l1.vault.accept_loop_end",
                    &format!("{e}"),
                    sock = sock_path.display()
                );
                break;
            }
        }
    }
}

fn handle_conn(
    mut stream: UnixStream,
    state: Arc<Mutex<HashMap<u64, Entry>>>,
) -> io::Result<()> {
    // Bound the handshake.  The body read below uses no timeout — a
    // single Deposit / Withdraw is the whole protocol; no long-lived
    // sessions to worry about.
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    server_handshake(&mut stream)?;
    stream.set_read_timeout(None)?;

    let (msg, payload, attached_fd) = recv_frame_with_fd(&stream)?;
    match (msg, attached_fd) {
        (VaultMsg::Deposit, Some(fd)) => handle_deposit(&mut stream, &state, &payload, fd),
        (VaultMsg::Deposit, None) => {
            let _ = send_frame_no_fd(
                &mut stream,
                VaultMsg::Error,
                b"Deposit without SCM_RIGHTS fd",
            );
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Deposit without SCM_RIGHTS fd",
            ))
        }
        (VaultMsg::Withdraw, _) => handle_withdraw(&mut stream, &state, &payload),
        (other, _) => {
            let _ = send_frame_no_fd(
                &mut stream,
                VaultMsg::Error,
                format!("expected Deposit/Withdraw, got {other:?}").as_bytes(),
            );
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("vault: unexpected first frame {other:?}"),
            ))
        }
    }
}

fn handle_deposit(
    stream: &mut UnixStream,
    state: &Arc<Mutex<HashMap<u64, Entry>>>,
    payload: &[u8],
    fd: OwnedFd,
) -> io::Result<()> {
    let (key, metadata) = match decode_deposit(payload) {
        Ok(v) => v,
        Err(e) => {
            let _ = send_frame_no_fd(stream, VaultMsg::Error, format!("{e}").as_bytes());
            return Err(e);
        }
    };
    let metadata_len = metadata.len();
    {
        let mut guard = state.lock().expect("vault mutex");
        // Overwrite is allowed: caller's choice.  Drop the old
        // OwnedFd closes its inherited fd (decrementing the
        // kernel-object refcount); if no one else holds the object,
        // the underlying PTY / shm / socket goes away.  That's the
        // intended semantic of a re-deposit on the same key.
        guard.insert(key, (fd, metadata));
    }
    lx_event!(
        "L1_VAULT_DEPOSIT",
        "fd deposited",
        key = key,
        metadata_len = metadata_len
    );
    send_frame_no_fd(stream, VaultMsg::DepositAck, &[])?;
    Ok(())
}

fn handle_withdraw(
    stream: &mut UnixStream,
    state: &Arc<Mutex<HashMap<u64, Entry>>>,
    payload: &[u8],
) -> io::Result<()> {
    let key = match decode_withdraw(payload) {
        Ok(k) => k,
        Err(e) => {
            let _ = send_frame_no_fd(stream, VaultMsg::Error, format!("{e}").as_bytes());
            return Err(e);
        }
    };
    let (raw_fd, metadata) = {
        let guard = state.lock().expect("vault mutex");
        match guard.get(&key) {
            Some((fd, meta)) => (fd.as_raw_fd(), meta.clone()),
            None => {
                let _ = send_frame_no_fd(
                    stream,
                    VaultMsg::Error,
                    format!("key {key} not found").as_bytes(),
                );
                lx_warn!("l1.vault.withdraw_miss", "no such key", key = key);
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("key {key}"),
                ));
            }
        }
    };
    // Dup the fd so the vault keeps its copy — a subsequent upgrade
    // can withdraw the same key again.  The caller's recvmsg gets a
    // fresh fd number in its own fd table; both refer to the same
    // kernel object.  Closes are independent (vault closes on
    // drain() or re-deposit; caller closes when its OwnedFd drops).
    let dup_fd = unsafe { libc::dup(raw_fd) };
    if dup_fd < 0 {
        let err = io::Error::last_os_error();
        let _ = send_frame_no_fd(
            stream,
            VaultMsg::Error,
            format!("dup failed: {err}").as_bytes(),
        );
        return Err(err);
    }
    let delivered_payload = encode_delivered(&metadata);
    send_frame_with_fd(
        stream,
        VaultMsg::Delivered,
        &delivered_payload,
        dup_fd,
    )?;
    // Caller's recvmsg pulls the dup into its own fd table; we close
    // our local copy.  Failure here doesn't lose data — the kernel
    // object stays alive via the caller's reference + the vault's
    // original.
    unsafe { libc::close(dup_fd) };
    lx_event!(
        "L1_VAULT_WITHDRAW",
        "fd delivered",
        key = key,
        metadata_len = metadata.len()
    );
    Ok(())
}

// Unused-import guard: depending on the rest of L1 wiring some of
// these symbols may not be hit by code-path linting yet.
#[allow(dead_code)]
fn _force_link() {
    let _ = fd_vault::VAULT_PROTO_VERSION;
}
