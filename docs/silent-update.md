# Silent update — architecture & operations

The marspot runtime ships as **three** independently-updatable
binaries.  Each has its own slot under
`~/Library/Caches/marspot/binaries/`:

```
binaries/
├── current/    ← active version
│   ├── marspot-shelld
│   ├── marspot-shell
│   └── marspot-core
├── prev/       ← rollback target (probation only)
├── pending/    ← updater drops new versions here
└── quarantine/ ← failed promotions (kept for diagnostics)
```

| Binary | What it owns | Update policy |
|---|---|---|
| `marspot-shelld` | PTY daemon, bytelogs, session lifetime | **Manual** — restart kills every session, requires explicit user consent via `bin/install-shelld.sh --apply-pending` |
| `marspot-shell` | NSWindow, IOSurface, supervisor state machine, banner overlay, control socket | **Silent** — promote + `execv` into new shell, same PID; window flashes closed→open in ~100 ms; sessions survive (shelld preserves bytelogs, new shell re-attaches) |
| `marspot-core` | Parser, terminal grid, render pipeline, input dispatch | **Silent** — promote + kill+respawn; IOSurface persists in shell, new core reattaches; ~50 ms freeze, no window flash |

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
                ┌─────┴────────────────────────────┐
                ↓                ↓                 ↓
       pending/shelld   pending/shell      pending/core
                │                │                 │
                │                │  focused(false) or SIGUSR1
                │                │   ↓
                │                │  shell.try_apply_shell_self_update
                │                │   ↓ promote (current→prev, pending→current)
                │                │   ↓ release IOSurface, kill core,
                │                │   ↓ drop control socket
                │                │   ↓ Command::exec into current/marspot-shell
                │                │   ─────────── execv ──────────────
                │                │   ↓
                │                │  new shell main() runs;
                │                │  MARSPOT_SHELL_SELF_UPDATE=1 → log
                │                │  reconnects shelld, reattaches sessions
                │                │   ↓
                │                │  if no pending shell, fall through to:
                │                │   ↓
                │                │  shell.apply_pending_update (core)
                │                │   ↓ promote core (current→prev, pending→current)
                │                │   ↓ spawn_core() into binaries/current/marspot-core
                │                │   ↓ 30 s probation
                │                │   ↓ stable → finalize_stable (delete prev/)
                │                │
                │   manual:      │
                │   bin/install-shelld.sh --apply-pending [--yes]
                │   ↓ prompt or --yes
                │   ↓ launchctl bootout
                │   ↓ promote (current→prev, pending→current)
                │   ↓ cp current/marspot-shelld → bundle path
                │   ↓ launchctl bootstrap
                ↓
       daemon restarted; SHELLD_UPDATE_STABLE logged.
```

## Failure modes

| What broke | What the supervisor does | What the user sees |
|---|---|---|
| New core dies inside 30 s probation | `PROBATION_FAIL` → `rollback_to_prev`: quarantine current, restore prev → current; respawn from rolled-back binary | Brief "Marspot is recovering…" banner; same content as before |
| New core dies + no prev to restore | `ROLLBACK_NOOP`; current is quarantined, `resolve_runnable` falls back to bundle sibling | Same as above; banner may flicker through a recovery cycle |
| 4 crashes in 5 min (rolling window) | `BUDGET_EXCEEDED`; `auto_restart_disabled=true`; no further respawns | Persistent "Marspot stopped — please restart the app" banner |
| New shell crashes immediately after exec | shell process dies, window closes | User re-opens Marspot.app; bundle binary journals each redirect into `shell_launches.tsv` and, on the 3rd launch of the same `current/` binary within 60 s, declares a crash loop: quarantines it, restores `prev/` (`SHELL_AUTO_ROLLBACK`), or runs as the bundle binary when no prev exists. Regression test: `bin/test-shell-rollback-loop.sh` |
| New shelld fails to bootstrap / dies inside 30 s probation | `bin/install-shelld.sh --apply-pending` polls `launchctl print` every 5 s for 30 s (`MARSPOT_SHELLD_PROBATION_S` overrides); not running at the end ⇒ `SHELLD_PROBATION_FAIL` → quarantine current, restore prev/ into current/ + bundle, re-bootstrap (`SHELLD_ROLLBACK`).  No prev/ ⇒ manual recovery | brief outage during probation; daemon back on the old version afterwards |
| Manual rollback (any reason) | `marspot-shell --rollback-shell` / `--rollback-core`: quarantine `current/`, restore `prev/` (`MANUAL_ROLLBACK`).  Runs offline in the bundle binary, before the current/ redirect, so it works even when current/ is the broken one | User restarts Marspot afterwards |

## Diagnostics

| Where | What |
|---|---|
| `~/Library/Logs/Marspot/supervisor.log` | TSV of every lifecycle event: STARTUP, CORE_SPAWN, HELLO_ACK, CRASH, UPDATE_APPLY, ROLLBACK, SHELL_SELF_UPDATE, SHELLD_UPDATE_APPLY, … |
| `marspot-shell --status` | Live PIDs + tail of supervisor.log with human-readable timestamps |
| `bin/install-shelld.sh --status` | Daemon plist + launchctl state + socket + pending shelld status |
| `bin/install-shell.sh --status` | Bundle executable + which binaries are installed |
| `binaries/quarantine/` | Last failed binaries; keep for crash report inspection |

## Release tarball schema

Updater extracts `marspot-shelld`, `marspot-shell`, `marspot-core` by
name from the downloaded tarball.  Sub-directory layout inside the
tarball doesn't matter — `find_named_file` recurses.  A v1-style
tarball with only `marspot-core` still works: shell + shelld pending
extracts silently no-op.

See `bin/build-release-tarball.sh` for the canonical packaging
script.

## Trust model

v1.1 (current): minisign-style detached-signature chain.

- Every release tarball ships with `<asset>.sig` — ECDSA P-256 over
  SHA-256, made by `bin/build-release-tarball.sh --sign` with
  `keys/marspot-update.sec` (gitignored; lives offline / in a GH
  secret).
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

(v1 was HTTPS + GitHub's SHA-256 asset digest; superseded.)

## Operations

```bash
# Cold install:
bin/install-shelld.sh           # daemon + LaunchAgent
bin/install-shell.sh            # shell+core into ~/.local/Marspot.app
open ~/.local/Marspot.app       # launch

# Check what's happening:
marspot-shell --status

# Force apply staged updates (skip focus-loss wait):
marspot-shell --trigger         # core + shell self-update
bin/install-shelld.sh --apply-pending --yes   # daemon (kills sessions!)

# All regression tests:
bin/test-all.sh                 # smoke + happy-path + rollback
bin/test-all.sh --soak          # …plus 60 s RSS soak
```

## Why the three-layer split

- **shelld** never updates without consent → `claude-code`,
  `tail -f`, watch loops survive every routine upgrade.
- **shell** updates rarely (only when supervisor logic / window
  policy changes) → momentary window flash is acceptable; user
  sees a one-frame transition, sessions resume.
- **core** updates frequently (every renderer / parser change) →
  silent re-attach is the dominant path; no visible flash.

The split is the load-bearing payoff of all the architecture work
in Steps 1-8: by isolating "what owns the window" from "what
renders into the window" from "what runs the user's shell," each
layer's update cost reflects what it actually does.
