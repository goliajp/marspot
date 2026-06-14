# Silent update — architecture & operations

## The four-layer split

The marspot runtime is split across **four** independently-updatable
binaries. The layer numbers (L1–L4) are the canonical names used
throughout code, logs, docs, and operator output:

| Layer | Binary | What it owns | Update policy |
|---|---|---|---|
| **L1** | `marspot-shell` | NSWindow, IOSurface, supervisor state machine, banner overlay, control socket | **Silent (opt-in)** — `install-local.sh --with-shell` triggers SIGUSR1 → promote + `execv` into new shell, same PID; window flashes closed→open ~100 ms; sessions survive (shelld preserves bytelogs, new shell re-attaches). Default `install-local.sh` skips L1 so no flash. |
| **L2** | `marspot-core` | Renderer, UI dispatch, layout, session bootstrap, control-socket protocol driving — **the marspot version** (title bar shows L2's version) | **Silent (default)** — dual-core swap; new core spawns alongside, presenter switches IOSurfaces, old core exits; ~50 ms freeze, no window flash |
| **L3** | `marspot-session` | Per-pane terminal engine: parser, grid, scrollback, shm publish, control reply | **Silent** — staged alongside core; new core boot-promotes `pending/marspot-session → current/` so the next L3 spawn picks up new bytes. `kill -USR2 <core-pid>` triggers in-place per-pane swap (idle panes silent, focused panes show a ↻ refresh affordance) |
| **L4** | `marspot-shelld` | PTY daemon, bytelogs, session lifetime | **Silent (opt-in)** — `install-local.sh --with-shelld` → `bin/install-shelld.sh --apply-pending-execv` → SIGUSR1; shelld promotes pending + `execv` over its own image in place, **preserving the listen socket fd + every PTY master fd + every session**. PID unchanged. Bootout/bootstrap fallback (`--apply-pending`) exists for daemons that predate the SIGUSR1 handler. |

Each layer carries its own semver in [`version-vector.toml`](../version-vector.toml).
L2 is the headline marspot version; the others bump only when their
own code surface changes (which is rare for L1/L4, periodic for L3).

The on-disk slot layout is shared across all four:

```
binaries/
├── current/    ← active version
│   ├── marspot-shell    (L1)
│   ├── marspot-core     (L2)
│   ├── marspot-session  (L3)
│   └── marspot-shelld   (L4)
├── prev/       ← rollback target (probation only)
├── pending/    ← updater drops new versions here
└── quarantine/ ← failed promotions (kept for diagnostics)
```

## shelld execv self-update — what makes sessions survive

The mechanism in `src/bin/marspot-shelld.rs::do_execv_swap`:

1. Snapshot every session's `(id, master_fd, child_pid)` into a tsv
   manifest on disk + write the path into `MARSPOT_SHELLD_HANDOFF`.
2. `BinaryTree::for_shelld().promote_pending()` — atomic rename of
   pending → current.
3. Clear `FD_CLOEXEC` on the listen fd + every PTY master fd, so they
   survive the image swap.  Set `MARSPOT_SHELLD_LISTEN_FD=<n>` so the
   new image knows which fd was the listener.
4. `libc::execv(current/marspot-shelld, [argv0])` — same PID, new
   image bytes.
5. New image's `main()`:
   - Reads `MARSPOT_SHELLD_HANDOFF` → parses the manifest.
   - Wraps the inherited listen fd back into a `UnixListener` (skips
     `bind()`).
   - For each session: `Pty::from_raw_master(fd, child_pid)` rebuilds
     the `Pty`, opens its bytelog in append mode, registers a fresh
     `ShellSession`, spawns a per-session reader thread.
   - Emits `EXECV_RESUME_BEGIN` / `EXECV_RESUME_DONE` events.
6. On `execv()` failure: rollback the binary tree (`prev/` → `current/`),
   re-set `FD_CLOEXEC`, remove the manifest file.  The original image
   keeps running — no session loss.

## Client-side reconnect — what makes GUI clients not blink

The shelld execv mechanism above only preserves the *daemon side* of
each connection.  The GUI's accepted-client fd was CLOEXEC by Rust std
default, so it cleanly EOFs the moment `execv()` lands.  Without a
client-side reconnect, `reader_loop` would see EOF and mark every
session exited — `marspot-core` would observe all panes exited and
break out of its event loop, the application would disappear.

`marspot_term::shelld_client::supervisor_loop` solves that:

1. Reader loop returns on EOF / read error (no longer marks sessions
   exited — that's now the supervisor's call).
2. Supervisor locks the writer `Mutex<UnixStream>` for the entire
   reconnect window so in-flight `send_frame` blocks instead of
   writing to a dead fd.
3. Reconnects with exponential backoff: 50 ms → 100 ms → 200 ms →
   400 ms → 800 ms → 1.6 s → 2 s × 3 (~9.2 s total).
4. On success: swap the new stream into the writer, re-send
   `Hello(PROTO_VERSION)`, then re-send `Attach(id)` for every session
   id the client still owns.  shelld's ATTACH path replays the
   bytelog so the local Terminal state is reconstructed exactly.
5. Only on backoff exhaustion does it mark sessions exited.

Net: a shelld execv swap looks to the GUI like a sub-second pause in
data, not a connection failure.

## Lifecycle

```
                ┌────────────┐
                │  GitHub    │   24 h poll
                │  Release   │   via updater::spawn
                └─────┬──────┘
                      ↓ download tarball (curl)
                      ↓ download <asset>.sig
                      ↓ openssl dgst -verify (P-256, embedded pubkey)
                      ↓ tar -xzf → extract by name
                      ↓ strip Gatekeeper xattrs
                ┌─────┴───────┬─────────────────┬───────────────────┐
                ↓             ↓                 ↓                   ↓
       pending/shelld   pending/shell      pending/core    pending/session
                │             │                 │                   │
                │             │  focused(false) or SIGUSR1          │
                │             │   ↓                                 │
                │             │  try_apply_shell_self_update        │
                │             │   ↓ promote (current→prev, pending→current)
                │             │   ↓ release IOSurface, kill core, drop ctl sock
                │             │   ↓ Command::exec into current/marspot-shell
                │             │   ─────────── execv ──────────────
                │             │   ↓
                │             │  new shell main() runs
                │             │   ↓
                │             │  shell.apply_pending_update (core)
                │             │   ↓ promote core (current→prev, pending→current)
                │             │   ↓ also: boot-promote pending/marspot-session
                │             │   ↓ spawn_core() into binaries/current/marspot-core
                │             │   ↓ 30 s probation
                │             │   ↓ stable → finalize_stable (delete prev/)
                │             │
                │   install-local.sh --with-shelld (DEFAULT path):
                │   ↓ stages pending/marspot-shelld
                │   ↓ bin/install-shelld.sh --apply-pending-execv
                │   ↓ cp pending → bundle (for future cold restart)
                │   ↓ SIGUSR1 daemon
                │   ↓ daemon do_execv_swap (in-place, PID unchanged)
                │   ↓ 30 s probation: poll pid, fail on respawn
                ↓
       daemon swapped; SHELLD_UPDATE_APPLY_EXECV + EXECV_INVOKE +
       EXECV_RESUME_BEGIN + SHELLD_UPDATE_STABLE logged.
```

## Failure modes

| What broke | What the supervisor does | What the user sees |
|---|---|---|
| New core dies inside 30 s probation | `PROBATION_FAIL` → `rollback_to_prev`: quarantine current, restore prev → current; respawn from rolled-back binary | Brief "Marspot is recovering…" banner; same content as before |
| New core dies + no prev to restore | `ROLLBACK_NOOP`; current is quarantined, `resolve_runnable` falls back to bundle sibling | Same as above; banner may flicker through a recovery cycle |
| 4 crashes in 5 min (rolling window) | `BUDGET_EXCEEDED`; `auto_restart_disabled=true`; no further respawns | Persistent "Marspot stopped — please restart the app" banner |
| New shell crashes immediately after exec | shell process dies, window closes | User re-opens Marspot.app; bundle binary journals each redirect into `shell_launches.tsv` and, on the 3rd launch of the same `current/` binary within 60 s, declares a crash loop: quarantines it, restores `prev/` (`SHELL_AUTO_ROLLBACK`), or runs as the bundle binary when no prev exists |
| shelld `execv()` returns errno | `do_execv_swap` rolls back the binary tree, restores CLOEXEC, removes the manifest — the original image keeps running.  `execv.failed` event with errno + target | No user-visible effect; subsequent retry can succeed |
| shelld dies during execv probation (PID change) | `install-shelld.sh --apply-pending-execv` detects launchctl-spawned new pid, logs `SHELLD_PROBATION_FAIL`, exits non-zero.  `install-local.sh` falls back to `--apply-pending` (bootout/bootstrap) | Sessions lost (same as old path) |
| New shelld fails to bootstrap (fallback path) | `bin/install-shelld.sh --apply-pending` polls launchctl for 30 s.  Not running at the end → `SHELLD_PROBATION_FAIL` → quarantine + restore prev + re-bootstrap | Sessions lost; daemon back on the old version |
| ShelldClient reconnect backoff exhausts | After ~9 s of reconnect failures the supervisor marks all sessions exited.  Core observes all-exited → exits cleanly.  marspot-shell sees core gone → quits | Application disappears (daemon is truly gone) |
| Manual rollback (any reason) | `marspot-shell --rollback-shell` / `--rollback-core`: quarantine `current/`, restore `prev/` (`MANUAL_ROLLBACK`).  Runs offline in the bundle binary, before the current/ redirect, so it works even when current/ is the broken one | User restarts Marspot afterwards |

## Diagnostics

Every event from every binary lands in **one** structured TSV stream
(`~/Library/Logs/Marspot/marspot.log`).  See `docs/logx.md` for the
runbook; key one-liners:

```bash
# Whole execv timeline
grep $'\tEXECV_' marspot.log | sort -t$'\t' -k2,2n

# Just the failures
grep -E $'\t(ERROR|WARN)\t' marspot.log

# By component
grep $'\tshelld\t' marspot.log
grep $'\tcore\t'   marspot.log
grep $'\tshell\t'  marspot.log

# Across rotations (for a multi-day incident)
{ cat marspot.log; for f in marspot.*.log.gz; do gunzip -c "$f"; done; } \
  | sort -t$'\t' -k2,2n
```

| Where | What |
|---|---|
| `~/Library/Logs/Marspot/marspot.log` | Structured TSV: pid / tid / ms / component / tag / msg / k=v fields per line.  Auto-rotates at 8 MiB → `.log.gz`, retains 10, GCs at 7 d.  Old `supervisor.log` files from prior installs are GC'd by `logx::gc::sweep_legacy_supervisor_log` after the same 7 d window. |
| `marspot-shell --status` | Live PIDs + tail of marspot.log filtered to lifecycle events |
| `bin/install-shelld.sh --status` | Daemon plist + launchctl state + socket + pending shelld status |
| `bin/install-shell.sh --status` | Bundle executable + which binaries are installed |
| `binaries/quarantine/` | Last failed binaries; keep for crash report inspection (GC'd after 30 d) |
| `~/Library/Logs/Marspot/shelld.log` / `shelld.err` | launchd-managed stdout/stderr from shelld — panic + pre-init safety net, never deleted by GC, tail-trimmed to 1 MiB once > 16 MiB |

## Release tarball schema

Updater extracts `marspot-shelld`, `marspot-shell`, `marspot-core`,
`marspot-session` by name from the downloaded tarball.  Sub-directory
layout inside the tarball doesn't matter — `find_named_file` recurses.
A tarball missing any binary just no-ops that pending slot.

See `bin/build-release-tarball.sh` for the canonical packaging
script.

## Trust model

v1.1 (current): minisign-style detached-signature chain.

- Every release tarball ships with `<asset>.sig` — ECDSA P-256 over
  SHA-256, made by `bin/build-release-tarball.sh --sign` with
  `keys/marspot-update.sec` (gitignored; also stored as the
  `MARSPOT_UPDATE_SEC` GitHub Actions secret for the release
  workflow to sign with).
- The public half is checked in at `keys/marspot-update.pub` and
  embedded in the updater at compile time (`include_str!`), so the
  trust anchor travels with the binary.  Verification:
  `/usr/bin/openssl dgst -sha256 -verify`.
- Releases with no `.sig` asset, or whose signature doesn't verify,
  are rejected before anything is staged.
- Why P-256 and not the originally-planned Ed25519: macOS's stock
  `/usr/bin/openssl` is LibreSSL 3.3 — no Ed25519 in
  `genpkey`/`pkeyutl`.  P-256 + `dgst` is the strongest scheme every
  supported macOS verifies with system tools alone (no new crates,
  per the self-build principle).
- Key rotation = new keypair + new release of the updater carrying
  the new pubkey; old updaters keep verifying old-key releases until
  upgraded through a release signed by the key they trust.

## Operations

```bash
# Install / update the terminal you actually use — builds, installs
# real bundle copies, then silent-updates the running app in place.
# Window + sessions survive throughout — including the daemon now,
# because --with-shelld defaults to the execv (--apply-pending-execv)
# path. The legacy bootout/bootstrap fallback only fires if the
# running shelld doesn't have a SIGUSR1 handler yet (cold migration).
bin/install-local.sh
bin/install-local.sh --with-shelld   # also bump the daemon (in-place execv)
bin/install-local.sh --status        # what's installed + running

# Iterate without touching the installed app — these run in a
# MARSPOT_STATE_DIR sandbox with their own shelld (see bin/_dev-sandbox.sh):
bin/run.sh                      # standalone marspot (tmux / bench / dev)
bin/test-all.sh                 # full shell+core regression suite

# Force apply staged updates (skip focus-loss wait):
marspot-shell --trigger                          # core + shell self-update
bin/install-shelld.sh --apply-pending-execv      # daemon (sessions preserved)
bin/install-shelld.sh --apply-pending --yes      # daemon (bootout, kills sessions)

# All regression / soak tests:
bin/test-all.sh                            # smoke + happy-path + rollback
bin/test-all.sh --soak                     # …plus 60 s RSS soak
bin/test-shelld-execv-swap.sh              # single shelld swap end-to-end
bin/soak-shelld-execv-swap.sh              # 3 sessions × 5 shelld swaps
bin/test-long-connection-execv.sh          # long-lived ShelldClient + 1 swap
bin/soak-long-connection-execv.sh          # long-lived ShelldClient + N swaps
bin/test-install-shelld-execv.sh           # install-shelld --apply-pending-execv

# Run the suite against the release profile (what production uses):
MARSPOT_TEST_PROFILE=release bin/test-shelld-execv-swap.sh
```

## Why the four-layer split

- **L1 marspot-shell** updates rarely (only when supervisor logic /
  window policy / probation timing changes) → opt-in via
  `install-local.sh --with-shell`; momentary NSWindow flash is
  acceptable for that explicit ask, sessions resume.
- **L2 marspot-core** updates frequently (every renderer / UI /
  protocol change) → silent dual-core swap is the dominant path; no
  visible flash. L2's version is THE marspot version.
- **L3 marspot-session** is per-pane → swapped per-pane via
  `kill -USR2 <core-pid>` on idle panes (silent); focused panes show
  a ↻ refresh affordance for explicit user action.
- **L4 marspot-shelld** updates in place via execv → `claude-code`,
  `tail -f`, watch loops survive every routine upgrade (the legacy
  bootout path stays as the fallback when execv isn't available).

The split is the load-bearing payoff of all the architecture work
in Steps 1-8: by isolating "what owns the window" (L1) from "what
renders into the window" (L2) from "what runs the user's shell" (L3)
from "what owns the PTYs" (L4), each layer's update cost reflects
what it actually does.
