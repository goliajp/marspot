# Mars — macOS terminal (pure Rust)

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

## Dependency audit (current)

| Crate | Status | Reason |
|---|---|---|
| `winit` | **on probation** | Convenience wrapper over AppKit. Replaceable with direct `objc2-app-kit` (~few hundred LOC for window + event loop). Should be removed when next touched. |
| `objc2`, `objc2-app-kit`, `objc2-foundation` | keep | FFI bindings to Apple frameworks — not "libraries that do work" |

Future planned bindings (`objc2-metal`, `libc` for PTY) will be FFI-only — keep.

## Dev workflow for AI sessions

After making changes that affect what the app does or how it builds, run `./bin/run.sh` to verify build + relaunch before reporting the task as done.

What counts as "affects the app":
- any change in `src/` (or any workspace crate when we add them)
- `Cargo.toml` / `Cargo.lock` (deps, features, profiles)
- shaders, assets

What doesn't:
- docs, comments, `.gitignore`, scripts unrelated to build, README

If `./bin/run.sh` fails, fix the build before claiming a task done. Full cargo output goes to `build/last-build.log`; only the last 60 lines echo on failure.

For perf testing use `./bin/run.sh --release`.

## Performance is the architecture

The first-principles project requirement: **mars must outperform iTerm2
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
  through `bin/measure.sh` and computes mars / best-other-terminal
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
mars's relative position needs re-validation.

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

## Architecture (planned, built incrementally)

- `src/main.rs` — entry, event loop, window management
- `src/term/` — terminal emulator (VT/xterm escape parser; self-built, grown iteratively against real apps)
- `src/render/` — Metal renderer with glyph atlas (instanced quads, custom)
- `src/pty/` — PTY management (libc syscalls directly, no wrapper crate)
- `src/scrollback/` — disk-backed scrollback persistence (mmap + line index)
- `src/tabs/` — multi-terminal lifecycle and shared GPU resources

These modules are created as features are added, not preemptively.
