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
