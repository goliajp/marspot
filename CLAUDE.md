# Marspot — macOS terminal (pure Rust)

> Product: **Marspot** (`marspot.com`). Repo:
> `~/workspace/goliajp/marspot`. Target: **v1.0.0 (self-use)**.
> Crate / lib / main binary / `Marspot` struct / `MARSPOT_*` env
> vars / `bench/baseline.json` keys all use the unified `marspot`
> name as of the post-migration rename. The companion single-session
> binary is `mcli` (kept short — "marspot cli").

## Engineering principles

These are non-negotiable unless explicitly overridden. They shape every code review, dependency decision, and architectural choice.

### 1. Self-build over libraries

- Beyond stdlib and necessary FFI bindings (objc2 family, libc, similar), prefer writing it ourselves
- Open-source libs may be **studied for reference**; depending on them is the exception, not the default
- Adding a new crate requires a written justification ("self-building is unreasonable because…")
- Bar for "necessary": months of work to replicate correctly, OR significant correctness risk (e.g., terminal escape-sequence spec edge cases)

### 2. Performance and resource efficiency are first-class

- Every PR is evaluated for: allocations, memory bounds, CPU at idle, latency tail
- **Idle CPU must be ~0%** — no animation timers, no busy waits, redraw only on dirty
- Hot paths (per-frame render, per-byte parser, key event) **allocate zero** — pre-allocate and reuse
- When perf goal conflicts with self-build principle, perf wins — and we document why

### 3. "Cannot get slower the longer it runs" — non-negotiable

This is the architecture-level commitment. Every long-lived data structure must have:

- **Bounded growth**: scrollback spills to disk; glyph atlas evicts via LRU; every `Vec`/`HashMap` whose size depends on user input has a documented cap or eviction strategy
- **Bounded queues**: PTY → parser → renderer channels are bounded with backpressure, not unbounded
- **Explicit ownership and Drop**: every fd, layer, texture, child process has a clear owner and tested teardown path; tab close releases everything
- **No background creep**: no timers that fire forever, no caches that accumulate, no thread pools that grow

We will write soak tests (run for hours, assert footprint stays bounded) before declaring any subsystem "stable".

### 4. Hard-won pieces become independent crates

When a sub-system inside marspot meets all three of:

- **Clean boundary**: well-defined input/output, no implicit reach into marspot internals
- **Earned by struggle**: real perf or correctness work went into it, the
  shape isn't obvious from a casual read of the public API
- **Plausibly reusable**: at least one external consumer exists in
  imagination — another macOS terminal, a side project, a benchmark tool

…it gets extracted into its own crate (or scripts repo for non-Rust
pieces), even at the cost of workspace overhead.  Reasons it's worth
the friction:

1. **Cleaner marspot architecture** — the boundary becomes load-bearing
   instead of "the file imports happen to work."  Latent coupling
   surfaces during extraction and gets fixed.
2. **Performance discipline** — once a crate has its own bench /
   soak, regressions can't sneak in via "incidentally a different
   caller used to pad it."
3. **Future-self leverage** — the pieces that took the most blood to
   get right (cell-sized glyph atlas, anon-mmap ring, multi-terminal
   bench harness) shouldn't have to be re-derived if a sibling
   project needs them.

The bar is "at least three of the above"; marspot-internal helpers that
fail any of those tests stay in `src/` as utility modules.

Extracted so far: `marspot-linkify` (2026-07-18 — clickable-span
detection: URL/path/email/IP/UUID over a `CellSource` trait; a month
of field-report-driven correctness work, zero deps, pure stdlib).

Current extraction backlog (judgement-call, not commitments):
`marspot-pty`, `marspot-anon-mmap-ring`, `marspot-glyph-atlas`, the
`bench-runner` shell-scripts repo.  These are the four pieces that
clearly clear the bar; everything else stays internal until/unless
it grows the same surface.

## Dependency audit (current)

| Crate | Status | Reason |
|---|---|---|
| `objc2`, `objc2-app-kit`, `objc2-foundation` | keep | FFI bindings to Apple frameworks — not "libraries that do work" |

Future planned bindings (`objc2-metal`, `libc` for PTY) will be FFI-only — keep.

## Dev workflow for AI sessions

After making changes that affect what the app does or how it builds, run `./bin/run.sh` to verify build + relaunch before reporting the task as done.

There are two distinct worlds; never let one disturb the other:

- **Installed app** (`~/.local/Marspot.app`) — the terminal the user actually lives in. Default state dir (`~/Library/Caches/marspot`), shelld via LaunchAgent. To ship a change into it, run `bin/install-local.sh`: it builds, installs real binary copies into the bundle, stages the changed shell/core into the supervisor's pending slots, and SIGUSR1-triggers the running app — the change lands in the user's window without losing sessions (and exercises the silent-update path every time). It refuses to touch shelld unless `--with-shelld` (that restart kills sessions).
- **Dev / test sandbox** — `bin/run.sh`, `bin/test-*.sh`, `bin/soak-*.sh` all `source bin/_dev-sandbox.sh`, which sets `MARSPOT_STATE_DIR` to a sandbox and runs a separate shelld there. Building, killing processes, wiping state, and crash-loop tests happen entirely in the sandbox; the installed app is never killed or wiped. This is enforced by `marspot::paths` (every state path keys off `MARSPOT_STATE_DIR`) and by `dev_kill_shell_core` (scoped pkill that can't match the bundle path).

So: iterate with `bin/run.sh` / tests freely, then `bin/install-local.sh` when you want it live. Do NOT hand-roll `pkill marspot` / `rm -rf ~/Library/Caches/marspot` in scripts — use the sandbox helpers.

What counts as "affects the app":
- any change in `src/` (or any workspace crate when we add them)
- `Cargo.toml` / `Cargo.lock` (deps, features, profiles)
- shaders, assets

What doesn't:
- docs, comments, `.gitignore`, scripts unrelated to build, README

If `./bin/run.sh` fails, fix the build before claiming a task done. Full cargo output goes to `build/last-build.log`; only the last 60 lines echo on failure.

For perf testing use `./bin/run.sh --release`.

## Performance is the architecture

The first-principles project requirement: **marspot must outperform iTerm2
and Warp on realistic use-cases**.  Performance is a property of the
architecture, not just the implementation — once the structure is wrong,
no amount of micro-optimisation reaches the ceiling.

Two living documents track this:

- `docs/architecture.md` — current module structure, hot paths,
  per-layer ownership, allocation budgets.  Updated alongside structural
  changes.
- `docs/perf.md` — use-cases, theoretical lower bounds, current
  measurements, gaps with fixability triage, gate construction.

### Benchmark gate

Two tiers:

- `bin/bench.sh` (fast, ~5 s) — headless `--bench parse` × 5 trials
  with median + `--bench render`. Run on every commit / before
  push. Catches any architectural regression in the parser, grid,
  or render bench.
- `bin/bench.sh --full` (~1–2 min) — also runs the live PTY pipeline
  through `bin/measure.sh` and computes marspot / best-other-terminal
  ratio against the snapshot in `bench/baseline.json`. Run before
  merging to develop, after any change touching the bytes path
  (parser, terminal, render) or the event loop (main.rs).
- `bin/bench.sh --update-baseline` — rewrites `bench/baseline.json`
  with current measurements minus a 7 % (headless) / 10 % (live)
  safety margin. Use after an intentional perf-affecting change
  whose new numbers you want to lock in.  Refuses if the gate is
  already failing.

`bin/measure-other.sh` keeps cross-terminal numbers fresh: dispatches
the same scenarios into iTerm2 and Warp via AppleScript, parses
timings, writes JSON.  Re-run when iTerm2 / Warp updates or when
marspot's relative position needs re-validation.

`bin/bench-remote.sh` runs the same gate on a clean idle Apple
Silicon host (default `ssh mini`) — same-arch, no foreground
jitter, dedicated `CARGO_TARGET_DIR`, lock + cleanup contract,
results land in `bench/remote-runs/<UTC-iso>/`.  Use when the dev
box is busy or when locking in a new baseline.

`bin/remote-measure-others.sh` is the companion that refreshes
`bench/baseline.json.competitors_snapshot` from the remote host.
ssh sessions on macOS cannot dispatch AppleEvents to GUI apps;
the script health-checks for that and either auto-runs or prints
the Screen-Sharing workaround.  When refreshing isn't possible
right now, `MARSPOT_BENCH_ALLOW_STALE_COMPETITORS=1 bin/bench-remote.sh
--full` runs the gate anyway (with a stamped warning) using the
existing snapshot.

### Correctness gate

`bin/fuzz.sh` runs libfuzzer targets in `fuzz/fuzz_targets/` (default
60 s per target, parametrise via `DURATION_S` / `TARGET`).  Current
targets:

- `parse_vt` — feeds arbitrary bytes to the VT/xterm parser through
  a no-op callback sink; asserts the parser never panics.  Stops
  spec-edge-case bugs at the input boundary.

`bin/miri.sh` runs Miri on the four pure-Rust modules (parser, grid,
tmux, input — 44 tests).  Miri cannot execute marspot's FFI
(libc::mmap/madvise, pthread, Metal, AppKit, CoreText), so the rest
of the lib is covered by real-process integration tests instead.  This
catches UB / dangling pointers / aliasing violations in the algorithmic
core — the class of bug that silently degrades long-running terminals
and is invisible to `cargo test`.

### Tests + profiling

`bin/test.sh` runs the lib suite via `cargo nextest run --lib`.
nextest's per-test process isolation makes individual failures
visible by name (vs. `cargo test`'s long combined output where one
panic can get lost among hundreds of pass lines), and parallel
scheduling cuts wall clock at ~140 tests.

`bin/profile-samply.sh` records a flamegraph-friendly profile via
samply (Speedscope / Firefox-profiler JSON) — the visual complement
to `bin/profile-live.sh`'s textual `/usr/bin/sample` report.  Default
runs an internal cat-ascii workload; `--attach <pid>` samples an
already-running marspot/mcli instead.

### Dep hygiene gate

`bin/lint-deps.sh` runs three peer checks on Cargo.toml / Cargo.lock:
`cargo audit` (RustSec advisories), `cargo deny check`
(license policy + duplicate-version warn + source provenance), and
`cargo machete` (unused declared deps).  Run pre-push alongside
`bin/bench.sh`.  Allowed licenses live in `deny.toml` and are
trimmed to exactly what current deps need — adding a license entry
is a forcing function to review whether the introducing dep is
justified under the self-build principle.

### Architecture-review cadence

Run before each merge to develop, when bench regresses, and proactively
every ~10 commits.  The checklist lives in `docs/architecture.md`.
Output goes in the merging commit / PR description:

- Did this change make architecture better, neutral, or worse?
- Hot-path delta: any new per-byte / per-frame allocations, branches,
  syscalls, locks?
- Did it leave headroom for future optimisation, or did it close it?
- Are "TODO: fix later" comments piling up? (if yes, schedule refactor)

If the answer is "made it worse": refactor first, then merge.  Or open
an explicit tech-debt task with a deadline.

### Micro-refactor as default

Don't wait for "the big refactor day."  Each PR gets a small structural
cleanup if any of these signals fire:

- Same code pattern appears ≥3 times
- Function exceeds ~80 lines
- Module boundary leaked (renderer reaches into PTY internals, etc.)
- State / buffers live in the wrong layer

If a commit includes a micro-refactor, mention it explicitly in the
commit message — it makes review fast and keeps the practice visible.

## Working with the user

**No multi-choice menus at decision points.**  Once a linear plan is
agreed (e.g. render-floor → metal-renderer → local-echo), keep
executing it.  At natural pauses, the only acceptable next-turn
prompts are:

  - **continue** (proceed with the next step of the agreed plan), or
  - **stop** (the user wants to halt), or
  - **adjust the plan** (the user wants to change scope / order)

Do NOT lay out 2–3 alternatives and ask the user to pick.  That
shifts the planning load back onto the user every turn — exactly
what the agreed plan was meant to absorb.  Either execute, or
say "next is X, continuing" and do it.  If genuinely blocked, say
what's blocking and stop — don't dress it up as a menu.

## Project conventions

- **Pure Rust binary**, no Swift / Xcode. Target is macOS only — we use Metal and AppKit directly via Rust bindings.
- git-flow (AVH) — production = `master`, integration = `develop`. New work goes on `feature/*`; finish via `git flow feature finish <name>` to fast-forward into `develop`.
- Deployment target: macOS 14.0+ (we'll add proper `.app` bundling and signing later).

### Commit scope policy (strict)

Every commit MUST start with one of three scopes — chosen so the
log makes the work area obvious without reading the diff:

| scope   | covers                                                                                                                                                                                                                                                  |
|---------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `infra` | the 4-layer architecture (shell / core / session / shelld), wire protocols, install-local + execv self-update, LaunchAgent + signal handling, storage (paths, bytelog, state.bin, scrollback disk), shelld daemon internals, logx, PluginHost / PluginRegistry framework |
| `basic` | terminal emulator (parser / grid / scrollback / cursor / SGR), render (Metal, glyph atlas, layout, selection, IME), pane / window / focus / sidebar / title strip, key + mouse + clipboard handling, generic PaneSession mechanism (wire + dispatcher + renderer hooks for badge / overlay) |
| `cc`    | claudecode-specific plugin code (`plugins/claudecode.rs`, `plugins/pidtree.rs`), profile-cycle state machine, API-error monitor, cc-specific badge text                                                                                                  |

**Format** — `<scope>: <subject>` on the title line.  Examples that
match this policy:

- `infra: shelld log SIGTERM sender pid via SA_SIGINFO`
- `basic: hit-test pane badge prefix on mouse_down, send PaneBadgeClicked`
- `cc: profile cycle switches from exit\r to SIGTERM`

**One commit, one scope.**  A change that touches more than one
scope gets split into separate commits — otherwise the scope tag
stops carrying signal.  When a boundary call is genuinely ambiguous,
prefer the deeper layer: framework code goes `basic`, business
glue goes `cc`.

The `RFC-NNN` step tag stays optional and lives after the scope —
`infra: RFC-002 step 8d — ATTACH carries cols/rows`.

## Architecture (planned, built incrementally)

- `src/main.rs` — entry, event loop, window management
- `src/term/` — terminal emulator (VT/xterm escape parser; self-built, grown iteratively against real apps)
- `src/render/` — Metal renderer with glyph atlas (instanced quads, custom)
- `src/pty/` — PTY management (libc syscalls directly, no wrapper crate)
- `src/scrollback.rs` — disk-backed scrollback (mmap'd ring file).  On
  whenever `MARSPOT_SESSION_ID` is set, i.e. in every L3 pane; mcli,
  `--snapshot` and tests fall through to the in-RAM variant.  The old
  `MARSPOT_DISK_SCROLLBACK=0` opt-out was removed after F1 soaked —
  documented here until 2026-08-19, and measured against twice before
  anyone noticed the switch did nothing
- `src/tabs/` — multi-terminal lifecycle and shared GPU resources

These modules are created as features are added, not preemptively.
