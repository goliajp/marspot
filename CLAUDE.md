# Mars — macOS terminal (pure Rust)

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
- `src/term/` — terminal emulator (will use the `alacritty_terminal` crate as engine)
- `src/render/` — Metal renderer with glyph atlas (instanced quads, custom)
- `src/pty/` — PTY management
- `src/scrollback/` — disk-backed scrollback persistence (mmap + line index)
- `src/tabs/` — multi-terminal lifecycle and shared GPU resources

These modules are created as features are added, not preemptively.
