# `logx` — structured logging, auto-rotation, cold-data GC

> "Next time something explodes at 2 a.m., you should not have to
>  reconstruct the timeline from `eprintln!` fragments."

## What this is

A single greppable TSV stream every marspot binary writes to:
`~/Library/Logs/Marspot/marspot.log` (or `$MARSPOT_STATE_DIR/logs/marspot.log`
in a sandbox). Each line is independently parseable, each field is
column-clean, the file rotates when it gets big, rotated files compress
to `.log.gz`, and cold data (old rotated logs, orphan session bytelogs,
old `binaries/prev` and `binaries/quarantine`) is GC'd in the background
without you ever running a script.

## Line format

```
<ISO-ms>\t<unix-ms>\t<LEVEL>\t<component>\t<pid>\t<tid>\t<tag>\t<msg>\t<k=v k=v ...>\n
```

Real example:

```
2026-06-14T09:57:29.160Z	1781431049160	INFO	shelld	64037	1f2b9de80	EXECV_INVOKE	calling libc::execv — outgoing image yields here	target=/tmp/marspot-logdemo.64033/binaries/current/marspot-shelld	listen_fd=4	n_sessions=0	cloexec_master_fail=0	elapsed_us=11970
```

- **ISO-ms** is for human eyes when you `tail -f`.
- **unix-ms** is for machine merging: `sort -t$'\t' -k2,2n` collates a
  union of files chronologically.
- **component**: `shell` / `core` / `shelld` / `session` / `gui`.
- **tag**: snake_case for routine events, `UPPER_SNAKE` for lifecycle.
  Filter via tag prefix: `grep $'\tEXECV_'` pulls the whole execv arc.
- **k=v fields**: keys are `[a-z0-9_.]+`; values get tab/newline/space
  replaced with `_` so columns stay clean.
- Lines are < 480 B (truncation marker `…trunc` appended past that),
  so cross-process `O_APPEND` writes are atomic per POSIX `PIPE_BUF`.

## Call-site convention

```rust
use marspot::{lx_info, lx_warn, lx_error, lx_event, lx_debug, lx_trace};

// Routine event:
lx_info!("session.reader.exit", "EOF on PTY", id = id, bytes = total);

// Lifecycle / promote-to-supervisor-event:
lx_event!("EXECV_INVOKE", "yielding to libc::execv",
          target = target.display(), listen_fd = fd, n_sessions = n);

// Error path:
lx_error!("execv.failed", &format!("{err}"),
          errno = errno, target = target.display());
```

Conventions for *new* call sites:

- **Tag**: lowercase `module.event` for routine work (`session.reader.exit`,
  `bind.ok`, `client.read_error`); UPPER_SNAKE for things that go on a
  supervisor timeline (`EXECV_INVOKE`, `BOOT_PROMOTE_OK`, `SHELLD_START`).
- **Fields > prose**: prefer `n_sessions=9` over `"with 9 session(s)"`.
  The grep will thank you.
- **Per-event level**:
  - `error!` — something went wrong and we degraded
  - `warn!`  — anomaly worth noticing; we kept going
  - `info!`  — every routine state change (default visible level)
  - `debug!` — useful when you're debugging that subsystem
  - `trace!` — per-syscall / per-byte; off in release by default

## Env vars

```
MARSPOT_LOG          = trace|debug|info|warn|error   (default info)
MARSPOT_LOG_<COMP>   = same                          (per-component override, wins over global)
MARSPOT_LOG_STDERR   = 0|warn|1                      (default 0; mirror to stderr at level)
MARSPOT_LOG_DIR      = override dir                  (default paths::log_dir())
MARSPOT_LOG_MAX_MB   = rotate size cap               (default 8)
MARSPOT_LOG_KEEP     = rotated backups kept          (default 10)
MARSPOT_LOG_GC_AGE_D = cold-data days cap            (default 7)
MARSPOT_LOG_GC       = 0|1                           (default 1)
```

`MARSPOT_LOG_<COMP>` is uppercased: `MARSPOT_LOG_SHELLD=debug`, `MARSPOT_LOG_GUI=trace`.

## Rotation

- Triggered on either size (`MAX_MB`, default 8 MiB) or age (24 h since
  the last rotate, so an idle daemon still produces a nicely-named
  daily file).
- The rotating process holds `flock(LOCK_EX | LOCK_NB)` on
  `marspot.log.rotate-lock` while it renames `marspot.log` → 
  `marspot.<nanos>.log` and creates a fresh active file. Sibling
  processes notice the inode change on their next size check and
  reopen onto the new active.
- A detached thread then compresses `<nanos>.log` → `<nanos>.log.gz`
  via `flate2::GzEncoder` (when the `compress` feature is on; default
  for everyone except `marspot-session`). On crash mid-compress, the
  orphan `.log` is GC'd by age.
- Retention: count-based by mtime — keep the `MARSPOT_LOG_KEEP` newest
  backups, prune the rest after every rotate.

## GC

Two entry points:

- **`gc::sweep_startup(component)`** — every binary calls this from
  `logx::init` in a detached thread. Light: only walks the shared
  log dir's rotated `.log` / `.log.gz` files.
- **`gc::sweep_full(&LiveSet)`** — only shelld calls this from its
  6-hour periodic tick (snapshots the live `Sessions` id set first).
  Adds the cross-cutting work:

  | Cold artifact | Trigger |
  |---|---|
  | Rotated structured logs | mtime > `MARSPOT_LOG_GC_AGE_D` (default 7d) |
  | Legacy `supervisor.log` | mtime > 7d |
  | Orphan `sessions_dir/<id>/bytelog` | `<id>` not in live set ∧ mtime > 1h grace |
  | `binaries/prev/*` | mtime > 7d (rollback window past) |
  | `binaries/quarantine/*` | mtime > 30d (forensic retention) |
  | `shelld.log` / `shelld.err` | size > 16 MiB → tail-trim to 1 MiB (never unlinked — pre-init safety net) |

  Protected (never touched by GC): active `marspot.log` (no `<nanos>`
  in its filename → never matches the rotated glob); `binaries/pending/`
  (user-driven, deleting it = upgrade lost); `binaries/current/`
  (running image).

## Runbook: "something just exploded, where do I look?"

1. **Quick smell**: `tail -200 ~/Library/Logs/Marspot/marspot.log`.
2. **Filter to errors**: `grep -E $'\t(ERROR|WARN)\t' ~/Library/Logs/Marspot/marspot.log`.
3. **Filter to one subsystem**:
   ```
   grep $'\tshelld\t' marspot.log
   grep $'\tEXECV_'   marspot.log    # the execv timeline
   grep $'\tBOOT_'    marspot.log    # boot-promote events
   ```
4. **Merge across rotations** (for a multi-day incident):
   ```
   {
     cat marspot.log
     for f in marspot.*.log.gz; do gunzip -c "$f"; done
   } | sort -t$'\t' -k2,2n
   ```
5. **Audit fd state across an execv** (the bug class this whole module
   was built for):
   ```
   grep -E $'\tEXECV_(HANDOFF_BEGIN|INVOKE|RESUME_BEGIN|RESUME_DONE|ROLLBACK_DONE)\t' marspot.log \
     | awk -F'\t' '{print $1, $4, $5, $7, $8, $9, $10, $11, $12, $13}'
   ```
   You'll see pid before / pid after / listen_fd / n_sessions / errno
   on one screen.

## Testing

Unit (`cargo test -p marspot-term --lib logx::`):

- Level filter + parse + line size + truncate + ISO-8601 conversion.
- Rotate-at-size-cap, skip-when-under-cap, retention prune by mtime.
- GC: rotated logs by age, name discrimination (active protected),
  orphan bytelog with live-set + 1h grace, launchd-log tail trim
  with newline snap.

Integration (`bin/test-log-concurrent.sh`):

- 50 parallel `marspot-shelld --log-event` invocations → asserts every
  tag appears once + no malformed (< 7 tab) line.

Soak (`bin/soak-log-rotate.sh`):

- `MARSPOT_LOG_MAX_MB=1 MARSPOT_LOG_KEEP=3`, 4-8 workers hitting
  `marspot-shelld --log-soak N` in a tight loop for 15-30 s.
- Asserts: bounded disk (≤ (KEEP+2) × MAX_MB × 1.3 MiB), backup count
  ≤ KEEP+slack, planted 8-day-old rotated file GC'd, total events
  preserved across active + rotated.

Run all three before merging anything that touches `logx::`.

## Adding a new event

```rust
// Don't:
eprintln!("[mymod] something happened: id={}, err={}", id, err);

// Do:
lx_warn!("mymod.something_failed", &format!("{err}"), id = id);
```

If it's a lifecycle marker that should appear on the supervisor
timeline (`UPDATE_APPLY`, `EXECV_INVOKE`, …):

```rust
lx_event!("MYMOD_THING_DONE", "human-readable summary",
          field_a = a, field_b = b);
```

Don't reach for `marspot::logx::event(...)` directly — the macros
short-circuit on `should_log` before evaluating `format!`, which is
the only reason this is cheap to leave at the call site of a hot path.

## Migration status

PR 1 (this commit set):
- ✅ Infrastructure: `logx.rs`, `logx/sink.rs`, `logx/rotate.rs`, `logx/gc.rs`
- ✅ `flate2` direct dep with `compress` feature (default-on; `marspot-session` opts out)
- ✅ `marspot-shelld` 35 eprintln! migrated; execv path fully structured
- ✅ `marspot-shelld --log-event` / `--log-soak` CLI hooks
- ✅ `sup_log.rs` becomes a 10-line shim over `logx::event`
- ✅ `bin/install-shelld.sh` `sup_log()` bash function routes through CLI
- ✅ shelld 6h `gc::sweep_full` tick
- ✅ Tests (15 unit) + concurrent integration + soak

Deferred to PR 2:
- `marspot-shell/main.rs` 49 eprintln! → `lx_*!` (lifecycle FSM
  paths to `lx_event!`, breadcrumbs to `lx_debug!`).
- `marspot-session/src/main.rs` 23 eprintln! → `lx_*!`.

Deferred to PR 3:
- `marspot-core.rs` + `src/main.rs` migration.
- Remove the legacy `supervisor.log` compat path after dogfooding.
