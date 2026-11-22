//! Starting a core process, sending to it and tearing it down.

use super::*;

impl ShellApp {
    pub(super) fn shutdown_active(&mut self, reason: &'static str) {
        match self.active.take() { Some(conn) => {
            let pid = conn.child.as_ref().map(|c| c.id()).unwrap_or(0);
            lx_event!(
                "ACTIVE_SHUTDOWN",
                "tearing down active core",
                reason = reason,
                pid = pid
            );
            conn.shutdown();
            frame_layer::set_core(None);
        } _ => {
            // No-op path is still worth logging — it tells us a
            // shutdown was requested when no core was live (race
            // between supervisor signals).
            lx_debug!(
                "shell.shutdown_active.noop",
                "shutdown_active called but slot was empty",
                reason = reason
            );
        }}
    }

    /// Record a CORE_SPAWN into the rolling ring and flag a
    /// boot-loop alarm if the ring's 30 s window now holds ≥3 boots.
    /// Cheap O(N) on a VecDeque bounded at 16 — the loop check is
    /// already paying VecDeque ops to maintain the ring; the
    /// occasional WARN is dwarfed by everything else in `poll_supervisor`.
    pub(super) fn record_core_boot(&mut self) {
        self.core_generation = self.core_generation.saturating_add(1);
        ownership::log_sessions();
        let now = Instant::now();
        let window = std::time::Duration::from_secs(30);
        let cutoff = now - window;
        while self
            .core_boot_ring
            .front()
            .map(|t| *t < cutoff)
            .unwrap_or(false)
        {
            self.core_boot_ring.pop_front();
        }
        self.core_boot_ring.push_back(now);
        // Cap the ring even if eviction lags (defensive).
        while self.core_boot_ring.len() > 16 {
            self.core_boot_ring.pop_front();
        }
        let count = self.core_boot_ring.len();
        if count >= 3 {
            lx_event!(
                "CORE_BOOT_LOOP",
                "core respawn loop detected — see CORE_SPAWN/CORE_EXIT timeline",
                count = count,
                window_s = 30
            );
            sup_log::log(
                "CORE_BOOT_LOOP",
                &format!("count={count} window_s=30"),
            );
        }
    }

    /// True when the active core has a live child process.  False
    /// before the first spawn, in the crash→respawn gap, and after a
    /// HELLO mismatch kill (where the conn lingers but `child` is gone).
    pub(super) fn core_alive(&self) -> bool {
        self.active
            .as_ref()
            .and_then(|c| c.child.as_ref())
            .is_some()
    }

    /// Send a frame to the active core.  Input, resize, ping, and
    /// focus all target `active` only — a `pending` core on probation
    /// receives none of them (it rebuilds independently from shelld).
    pub(super) fn send(&self, msg_type: MsgType, payload: Vec<u8>) {
        // whatever the core is sent, short of a ping, is something happening
        if let Some(p) = self.pump.as_ref().filter(|_| msg_type != MsgType::Ping) {
            p.touch();
        }
        if let Some(conn) = self.active.as_ref() {
            conn.send(msg_type, payload);
        }
    }

    /// Spawn a core process rendering into `surface_id` and return the
    /// `CoreConn` (HELLO already sent on its own socket).  Returns
    /// `None` on any spawn failure.  The caller decides whether the new
    /// core becomes `active` (boot / restart) or `pending` (an update on
    /// probation) — this fn touches neither slot.
    pub(super) fn spawn_core(
        &self,
        front_id: u32,
        back_id: u32,
        w_phys: usize,
        h_phys: usize,
        scale: f64,
    ) -> Option<CoreConn> {
        // Resolve via the supervisor binary tree:
        //   1. MARSPOT_CORE_BIN env override (full path or sibling
        //      name — useful in dev / when pointing at an alt core).
        //   2. `~/Library/Caches/marspot/binaries/current/marspot-core`
        //      if a prior silent update has staged one.
        //   3. Sibling of `marspot-shell` (dev / first-run install).
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                lx_error!("shell.current_exe_failed", &format!("{e}"));
                return None;
            }
        };
        let core_bin = self.binaries.resolve_runnable(&exe);

        // Create the bidirectional control socket BEFORE spawn so the
        // child can inherit one end as fd 3.  socketpair(AF_UNIX,
        // SOCK_STREAM) gives us two fds in the parent: parent_fd keeps
        // the shell's side; child_fd gets dup2'd to 3 in pre_exec, then
        // closed in the parent after spawn returns.
        let mut sp = [0i32; 2];
        let r = unsafe {
            libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sp.as_mut_ptr())
        };
        if r != 0 {
            lx_error!(
                "shell.socketpair_failed",
                &format!("{}", std::io::Error::last_os_error())
            );
            return None;
        }
        let parent_fd: RawFd = sp[0];
        let child_fd: RawFd = sp[1];

        // CLOEXEC both ends so the core child never inherits the shell
        // (parent) end of its own control socket: without this the core
        // holds both ends, so fd 3 never sees EOF when the shell dies and
        // the core — plus its whole L3 tree — orphans instead of exiting
        // (a 1 core + N session leak per shell crash/restart). The
        // pre_exec dup2 re-clears CLOEXEC on the child's fd 3 below, so
        // the core still gets its control socket. A pending core spawned
        // during a dual-core update likewise won't inherit the active
        // core's control end.
        for fd in [parent_fd, child_fd] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags >= 0 {
                unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
            }
        }

        lx_info!(
            "shell.core.spawning",
            "spawning marspot-core",
            bin = core_bin.display(),
            front_id = front_id,
            back_id = back_id,
            w = w_phys,
            h = h_phys,
            scale = scale,
            control_fd = DEFAULT_CONTROL_FD
        );
        let mut cmd = Command::new(&core_bin);
        cmd.env(ENV_SURFACE_ID, front_id.to_string())
            .env(ENV_SURFACE_ID_BACK, back_id.to_string())
            .env(ENV_SURFACE_WIDTH, w_phys.to_string())
            .env(ENV_SURFACE_HEIGHT, h_phys.to_string())
            .env(ENV_SURFACE_SCALE, scale.to_string())
            .env(ENV_CONTROL_FD, DEFAULT_CONTROL_FD.to_string())
            .env(marspot::ui::system::macos::icons::FROM_SHELL_ENV, "1");
        // SAFETY: pre_exec runs in the forked child between fork and
        // exec.  Only async-signal-safe libc calls are allowed; we
        // only use dup2/close/fcntl which are all on the AS-safe list.
        unsafe {
            cmd.pre_exec(move || {
                if child_fd != DEFAULT_CONTROL_FD {
                    if libc::dup2(child_fd, DEFAULT_CONTROL_FD) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::close(child_fd);
                }
                // Strip CLOEXEC so the core sees fd 3 after exec.
                let flags = libc::fcntl(DEFAULT_CONTROL_FD, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(
                        DEFAULT_CONTROL_FD,
                        libc::F_SETFD,
                        flags & !libc::FD_CLOEXEC,
                    );
                }
                Ok(())
            });
        }
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                lx_event!(
                    "CORE_SPAWN",
                    "spawned marspot-core",
                    pid = pid,
                    bin = core_bin.display()
                );
                sup_log::log(
                    "CORE_SPAWN",
                    &format!("pid={pid} bin={}", core_bin.display()),
                );
                // Parent no longer needs the child end.
                unsafe { libc::close(child_fd) };
                // Wrap the parent end as a UnixStream we can write
                // frames to from any callback.
                let stream = unsafe { UnixStream::from_raw_fd(parent_fd) };
                frame_layer::set_core(stream.try_clone().ok());
                let reader_stream = match stream.try_clone() {
                    Ok(s) => s,
                    Err(e) => {
                        lx_error!(
                            "shell.control_stream.try_clone_failed",
                            &format!("{e}")
                        );
                        let mut child = child;
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                };

                // Spawn the reader thread.  Decodes frames into
                // ShellInbox messages, hands them to the main thread
                // via mpsc + `EventProxy::wake`.
                let (tx, rx): (Sender<ShellInbox>, Receiver<ShellInbox>) = mpsc::channel();
                let proxy = self.proxy.clone();
                std::thread::spawn(move || control_reader_loop(reader_stream, tx, proxy));

                // Liveness handshake bookkeeping — a fresh core gets a
                // fresh probe window.  Nonce starts at 0 (bumped to 1
                // before the first ping); this conn's reader owns its
                // own channel, so a previous core's Pong can't alias it.
                let now = Instant::now();
                let conn = CoreConn {
                    child: Some(child),
                    control_tx: Mutex::new(stream),
                    control_rx: rx,
                    hello_acked: false,
                    paced: false,
                    early_ready: Vec::new(),
                    spawned_at: now,
                    next_ping_at: now + PING_INTERVAL,
                    last_ping_nonce: 0,
                    last_pong_at: now, // freebie until first ping
                };

                // Send HELLO on this core's own socket so it can echo
                // HelloAck — this works whether the conn ends up active
                // or pending.
                // before the Hello: see `shell_proto::frame_pace`
                conn.send(MsgType::FramePace, Vec::new());
                conn.send(MsgType::Hello, encode_hello(PROTO_VERSION));
                Some(conn)
            }
            Err(e) => {
                lx_error!("shell.core.spawn_failed", &format!("{e}"));
                unsafe {
                    libc::close(parent_fd);
                    libc::close(child_fd);
                }
                None
            }
        }
    }
}
