# Marspot — macOS terminal (pure Rust)

Product **Marspot** (`marspot.com`). Pure Rust, macOS 14.0+ only, Metal and AppKit through Rust bindings — no Swift / Xcode. Everything uses the unified `marspot` name (crate, lib, binary, `MARSPOT_*` env vars, `bench/baseline.json` keys); `mcli` is the single-session companion binary.

## Layout

Three layers: L1 `marspot-shell` (`src/bin/marspot-shell/`, window owner + supervisor + plugins), L2 `marspot-core` (`src/bin/marspot-core.rs` over the `src/` lib: `render_metal.rs`, glyph atlas, fonts, `ui/`, panes), L3 `marspot-session` (`crates/marspot-session`, one process per pane, owns its PTY). The old L4 `marspot-shelld` daemon was retired by RFC-003 (`docs/rfc-003-l3-pty.md`); "shelld" in older comments and scripts refers to it. The zero-GUI engine (parser, grid, terminal, pty, disk scrollback, paths, logx) is `crates/marspot-term`; `crates/marspot-linkify` is the extracted clickable-span detector. Current structure, hot paths and allocation budgets: `docs/architecture.md`; perf targets and gaps: `docs/perf.md`.

## Engineering principles

- **Build over depend.** Beyond stdlib and FFI bindings (libc, objc2 family, core-* bindings), write it ourselves; open-source libs are for reading. A new crate needs a written "self-building is unreasonable because…" — put it as a comment next to the dep in `Cargo.toml`, as the existing ones are. Current deps: the `[dependencies]` tables of `Cargo.toml` and `crates/*/Cargo.toml`.
- **Performance is first-class.** Idle CPU ~0% (no animation timers, no busy waits, redraw only on dirty); hot paths (per-frame render, per-byte parse, key event) allocate nothing. When perf conflicts with build-over-depend, perf wins and the reason is written down.
- **Cannot get slower the longer it runs.** Every long-lived structure has bounded growth (scrollback spills to disk, atlas evicts, input-sized collections have a cap), channels are bounded with backpressure, every fd / texture / child has an owner and a tested teardown, and nothing grows in the background. Soak tests before calling a subsystem stable.
- Hard-won, cleanly bounded, plausibly reusable pieces get extracted into their own crate — criteria and backlog in `.claude/runbooks/crate-extraction.md`.

## Sandbox vs installed app — red line

- **Installed app** `~/.local/Marspot.app` is the terminal the user lives in (state `~/Library/Caches/marspot`). Ship into it only with `bin/install-local.sh`: it builds, installs and silent-updates the running app; sessions survive via L3 reattach.
- **Sandbox**: `bin/run.sh`, `bin/test*.sh`, `bin/soak-*.sh` source `bin/_dev-sandbox.sh` (own `MARSPOT_STATE_DIR`, default `/tmp/marspot-dev`). Build, kill, wipe and crash-loop only there. Never hand-roll `pkill marspot` or `rm -rf ~/Library/Caches/marspot`.
- After a change that affects the app (anything in `src/` or `crates/`, `Cargo.toml` / `Cargo.lock`, shaders, assets), `./bin/run.sh` must pass before calling it done (full log: `build/last-build.log`; perf testing: `--release`).

## Gates (one command each; details in `.claude/runbooks/gates.md`)

- tests: `bin/test-remote.sh` (runs `bin/test.sh` — nextest, sandboxed state dir — on mini; don't run nextest directly)
- bench: `bin/bench.sh` per commit; `bin/bench-remote.sh --full` before merging bytes-path or event-loop changes (only mini numbers count)
- correctness: `bin/fuzz.sh`, `bin/miri.sh`
- deps: `bin/lint-deps.sh` pre-push

## Working with the user

No multiple-choice menus at decision points. Once a plan is agreed, keep executing it; at a pause say "next is X, continuing" and do it. If genuinely blocked, say what blocks and stop.

## Git

git-flow-next: `develop` is integration, `master` only via release / hotfix finish; work on `feature/*`, land with `git merge --no-ff` into `develop`. No pull requests.

### Commit scope (strict)

Every commit title is `<scope>: <subject>` with exactly one of:

| scope | covers |
|---|---|
| `infra` | the layer architecture (shell / core / session), wire protocols, install-local + execv self-update, LaunchAgent + signal handling, storage (paths, bytelog, state.bin, scrollback disk), logx, PluginHost / PluginRegistry framework |
| `basic` | terminal emulator (parser / grid / scrollback / cursor / SGR), render (Metal, glyph atlas, layout, selection, IME), pane / window / focus / sidebar / title strip, key + mouse + clipboard, generic PaneSession mechanism (wire + dispatcher + renderer hooks for badge / overlay) |
| `cc` | agent-specific plugin code (`src/bin/marspot-shell/plugins/` — `claudecode.rs`, `codex.rs`, `autorun.rs`, `handoff/`), `src/cc.rs`, `src/cc_usage.rs`, `src/pidtree.rs`, profile-cycle state machine, API-error monitor, agent badge text |

One commit, one scope — split changes that span two. When ambiguous, prefer the deeper layer (framework → `basic`, business glue → `cc`). An optional `RFC-NNN` tag goes after the scope: `infra: RFC-002 step 8d — ATTACH carries cols/rows`.

## 按需手册(`.claude/runbooks/`)

| 文件 | 什么时候读 |
|---|---|
| `gates.md` | 跑或改 bench / fuzz / miri / test / lint-deps;架构评审节奏;沙箱与安装版的完整说明 |
| `perf-attack.md` | 任何性能攻坚之前(对照对象、mini 才算数、红线优先级;门槛数值读 `bench/baseline.json`) |
| `crate-extraction.md` | 想把子系统拆成独立 crate |
| `architecture-history.md` | 旧架构草图的遗留说明(disk scrollback 开关) |
