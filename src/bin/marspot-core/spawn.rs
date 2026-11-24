//! Starting L3 session processes for panes, with a cwd, inline or on a worker thread.

use super::*;

/// Absolute ceiling on live `marspot-session` processes this state
/// root may hold, counted against the registry at spawn time.
///
/// 2026-07-28 incident — the machine went down with 176 of them.
/// `SESSION_COUNT_HARD_CAP` (36) bounds panes per window, but nothing
/// bounded the PROCESS population: sessions outlive their windows by
/// design, so a crash-restore loop grew a new generation every cycle.
/// Refusing here turns "the system dies" into "one spawn fails and a
/// vacant pane says so" — the failure the architecture already knows
/// how to show.
const SESSION_PROCESS_HARD_CAP: usize = 64;

/// What a shadow pane's L3 is told at its start: whose shadow it is
/// and how long it may sit idle (0: its default).
#[derive(Clone, Copy, Debug)]
pub(super) struct ShadowEnv {
    pub(super) parent: u64,
    pub(super) idle_s: u32,
}

fn spawn_l3_with_cwd(
    cols: u16,
    rows: u16,
    session_id: u64,
    initial_cwd: &str,
    shadow: Option<ShadowEnv>,
    event_tx: &Sender<CoreEvent>,
) -> std::io::Result<L3Spawn> {
    // Population guard.  A readdir + one liveness check per entry, only on
    // the spawn path — spawns are user-rate (boot, [+], revive), never
    // per-frame.  `MARSPOT_SESSION_CAP` overrides the cap for tests
    // (spawning 64 real processes to prove a refusal would itself be
    // a small process storm).
    let cap = std::env::var("MARSPOT_SESSION_CAP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(SESSION_PROCESS_HARD_CAP);
    let live = marspot_term::session_registry::list_session_entries()
        .iter()
        .filter(|e| marspot_term::session_registry::entry_is_live(e))
        .count();
    if live >= cap {
        lx_error!(
            "core.spawn.session_population_cap",
            "refusing to spawn: live session processes at hard cap",
            live = live,
            cap = cap,
            session = session_id
        );
        return Err(std::io::Error::new(
            std::io::ErrorKind::QuotaExceeded,
            format!("{live} live sessions >= hard cap {cap}"),
        ));
    }
    // Under a name, so a core started after this one can find the
    // session's frames; the L3 repeats the name in entry.toml.
    let regions = grid_shm::SessionRegions::for_session(session_id);
    let shm_name = regions.grid_name().to_string_lossy().into_owned();
    let region = regions.create(cols, rows)?;
    let region_raw = region.as_raw_fd();

    // CLOEXEC the shm fd we hold here so it can't leak into a sibling
    // L3 spawned later.  The pre_exec dup2 below re-clears CLOEXEC on
    // fd 4 so the actual child still sees it.
    {
        let flags = unsafe { libc::fcntl(region_raw, libc::F_GETFD) };
        if flags >= 0 {
            unsafe { libc::fcntl(region_raw, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        }
    }

    // marspot-session lives next to marspot-core; an explicit override
    // (dev / tests) wins.
    let session_bin = match std::env::var_os("MARSPOT_SESSION_BIN") {
        Some(p) => std::path::PathBuf::from(p),
        None => std::env::current_exe()?
            .parent()
            .map(|d| d.join("marspot-session"))
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "no current_exe parent")
            })?,
    };

    const SHM_TARGET_FD: RawFd = 4;
    lx_event!(
        "L3_SPAWN",
        "spawning L3 session (UDS-only control, RFC-003 step 3b)",
        bin = session_bin.display(),
        cols = cols,
        rows = rows,
        shm_fd = SHM_TARGET_FD,
        session_id = session_id
    );
    let mut cmd = Command::new(&session_bin);
    cmd.env(ENV_SHM_FD, SHM_TARGET_FD.to_string())
        // L2 owns session assignment: hand this L3 the exact session it
        // must drive so N children never race for the same one.
        .env("MARSPOT_SESSION_ID", session_id.to_string())
        // Amendment 7 step 3: hand the L3 the shm name we chose so it
        // records it in entry.toml.  A future L2 then knows what to
        // shm_open for reattach.
        .env("MARSPOT_SHM_NAME", &shm_name);
    // F3+6 — pass the user's saved cwd through so the shell forks
    // there instead of $HOME.  Empty = unset (L3 falls back to $HOME).
    if !initial_cwd.is_empty() {
        cmd.env("MARSPOT_INITIAL_CWD", initial_cwd);
    }
    // the L3 reads these once and removes them; never inherited from ours
    cmd.env_remove("MARSPOT_SHADOW_OF").env_remove("MARSPOT_SHADOW_IDLE_S");
    if let Some(sh) = shadow {
        cmd.env("MARSPOT_SHADOW_OF", sh.parent.to_string())
            .env("MARSPOT_SHADOW_IDLE_S", sh.idle_s.to_string());
    }
    // Ensure the child does NOT inherit any control-socket env from
    // the L2 parent — RFC-003 step 3b leaves the inherited-fd path
    // behind entirely.
    cmd.env_remove(ENV_CONTROL_FD);
    // SAFETY: pre_exec runs between fork and exec; only async-signal-safe
    // libc calls (dup2/close/fcntl) are used.
    let close_limit = marspot_term::inherited_fds::close_limit();
    unsafe {
        cmd.pre_exec(move || {
            if region_raw != SHM_TARGET_FD
                && libc::dup2(region_raw, SHM_TARGET_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            let flags = libc::fcntl(SHM_TARGET_FD, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(SHM_TARGET_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
            // the frame region is the one descriptor L3 is meant to have
            marspot_term::inherited_fds::close_inherited(close_limit, &[SHM_TARGET_FD]);
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    lx_event!("L3_SPAWNED", "L3 child running", pid = child.id());

    // Map the region as a reader (mmap survives the fd closing, so the
    // owned `region` can drop after).
    let reader = GridShmReader::from_fd(region_raw)?;
    drop(region);

    // RFC-003 step 3b: connect to the L3's UDS instead of inheriting a
    // socketpair.  L3 boots, binds sessions/<id>/sock, writes entry.toml;
    // we poll for the entry then handshake.  Failure rolls back the
    // child (Drop sends SIGKILL via std).
    // 10 s, explicitly.  This used to read 5 s and get 10 in practice,
    // because `wait_and_connect` ran a fresh deadline for each of its
    // two halves.  Now that the deadline is honestly single, the number
    // here has to be the real budget — and a booting L3 needs it: it
    // execs, then applies a state.bin that can run into the hundreds of
    // KB before it binds its socket.  Halving it silently turned a slow
    // boot into a lost pane (caught by the boot-assembly tests).
    let control = match marspot::uds_session_client::wait_and_connect(
        session_id,
        std::time::Duration::from_secs(10),
    ) {
        Ok(s) => s,
        Err(e) => {
            lx_error!(
                "core.l3.uds_connect_failed",
                &format!("{e}"),
                session_id = session_id
            );
            // RFC-003 §6 Amendment 15.2 — std::process::Child::drop
            // is a no-op (Rust deliberately doesn't reap behind your
            // back); on this error path the just-spawned L3 would
            // otherwise live on forever as an orphan.  Explicit
            // SIGKILL + reap so we don't leak processes (and the
            // associated PTY + vault deposit, which the kernel
            // collects when the last reference drops).
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    let reader_stream = control.try_clone()?;
    let tx = event_tx.clone();
    let (selection_tx, selection_rx) = std::sync::mpsc::channel::<(u32, String)>();
    std::thread::spawn(move || l3_reader_loop(reader_stream, session_id, tx, selection_tx));

    Ok(L3Spawn {
        child,
        control,
        reader,
        regions,
        selection_rx,
    })
}

/// Start a pane whose L3 is brought up **off** this loop.
///
/// Returns immediately with a slot rendering "starting…"; the fork,
/// shm map, entry.toml poll and handshake all run on a worker, and
/// `CoreEvent::L3SpawnFinished` delivers the result.  Boot assembly
/// keeps calling `spawn_l3_pane_with_cwd` synchronously — there is no
/// loop to freeze before the loop starts, and boot wants the pane fully
/// formed before it lays out.
///
/// An empty `cwd` is the L3 default, which is `$HOME`.
/// Where a pane's shell currently is.
///
/// A new pane opens where the one you were in is, which is what every
/// other terminal does and what makes [+] useful next to a build.  The
/// answer has to come from the shell's own process: the registry
/// records where the session was *launched*, and every pane that has
/// since `cd`'d somewhere would open in the wrong place.
///
/// One `proc_pidinfo` per new pane.  This is the reason OSC 7 is not
/// wired up — a push-based cwd report costs a parse on every prompt,
/// and this costs one syscall when a pane is actually created.
///
/// Empty when the pid is gone or the call is refused, which is the
/// same string the caller passed before and means `$HOME`.
pub(super) fn live_cwd_of(session_id: u64) -> String {
    marspot_term::session_registry::read_session_entry(session_id)
        .ok()
        .and_then(|e| marspot::pidtree::proc_cwd(e.shell_child_pid))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub(super) fn spawn_l3_pane_async(
    cols: u16,
    rows: u16,
    session_id: u64,
    cwd: String,
    event_tx: &Sender<CoreEvent>,
) -> Pane {
    spawn_l3_pane_async_as(cols, rows, session_id, cwd, None, event_tx)
}

/// `spawn_l3_pane_async`, for a shadow pane when `shadow` names its parent.
pub(super) fn spawn_l3_pane_async_as(
    cols: u16,
    rows: u16,
    session_id: u64,
    cwd: String,
    shadow: Option<ShadowEnv>,
    event_tx: &Sender<CoreEvent>,
) -> Pane {
    let tx = event_tx.clone();
    std::thread::Builder::new()
        .name(format!("l2-spawn-{session_id}"))
        .spawn(move || {
            let inner_tx = tx.clone();
            let outcome = spawn_l3_with_cwd(cols, rows, session_id, &cwd, shadow, &inner_tx)
                .map_err(|e| format!("{e}"));
            let _ = tx.send(CoreEvent::L3SpawnFinished(
                session_id,
                SpawnOutcome(outcome),
            ));
        })
        .expect("spawn l2-spawn worker");
    Pane::new_pending(session_id, cols, rows)
}

/// RFC-005 step 6b — assemble a restored window's panes on a worker
/// thread and deliver them whole via `CoreEvent::WindowRestoreFinished`.
///
/// Off-loop is not an optimisation here, it is the rule: reattaching an
/// L3 blocks on a UDS handshake with its own deadline, and RFC-005 says
/// opening a window must never freeze the windows already up.  The
/// window appears immediately holding one "starting…" pane per saved
/// slot; the real panes replace them when the worker lands.
///
/// `Sweep::Off` — see `assemble_panes_at_boot`.
pub(super) fn assemble_restore_window_async(
    window_id: u32,
    record: marspot::state::SavedWindowLayout,
    cols: u16,
    rows: u16,
    event_tx: &Sender<CoreEvent>,
) {
    let tx = event_tx.clone();
    std::thread::Builder::new()
        .name(format!("l2-restore-w{window_id}"))
        .spawn(move || {
            let n = record.panes.len().clamp(1, SESSION_COUNT_HARD_CAP);
            let inner_tx = tx.clone();
            let (panes, _reattached) = assemble_panes_at_boot(
                Some(&record),
                n,
                cols,
                rows,
                &inner_tx,
                Sweep::Off,
                &std::collections::HashSet::new(),
            );
            let _ = tx.send(CoreEvent::WindowRestoreFinished(
                window_id,
                RestoredPanes(panes),
            ));
        })
        .expect("spawn l2-restore worker");
}

pub(super) fn spawn_l3_pane_with_cwd(
    cols: u16,
    rows: u16,
    session_id: u64,
    initial_cwd: &str,
    event_tx: &Sender<CoreEvent>,
) -> std::io::Result<Pane> {
    let spawn = spawn_l3_with_cwd(cols, rows, session_id, initial_cwd, None, event_tx)?;
    Ok(Pane::new_l3(L3Conn::new(spawn, session_id)))
}

/// A shadow pane's L3, started inline (tests).
#[cfg(test)]
pub(super) fn spawn_l3_shadow_with_cwd(
    cols: u16,
    rows: u16,
    session_id: u64,
    shadow: ShadowEnv,
    event_tx: &Sender<CoreEvent>,
) -> std::io::Result<Pane> {
    let spawn = spawn_l3_with_cwd(cols, rows, session_id, "", Some(shadow), event_tx)?;
    Ok(Pane::new_l3(L3Conn::new(spawn, session_id)))
}

