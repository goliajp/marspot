# Marspot bench mechanism

This is the **persistent benchmark system** that validates Marspot's
first-principles value: ultra-high-performance unlimited scrolling
across many concurrent terminal sessions, used as a working surface for
multi-session Claude-Code development.

It must answer, on demand and over time:

1. Is marspot faster than iTerm2 / Warp on every realistic use-case the
   user actually hits? (Terminal.app is the OS-vendor reference floor.)
2. Does marspot hold those numbers steady — across releases, across hours
   of uptime, across a 9-session day?
3. When something regresses, can we localise the cause without
   guessing?

Everything in `bin/bench.sh`, `bin/measure*.sh`, `bench/scenarios/` and
`bench/results/` exists in service of those three questions. Read this
doc before adding a metric, scenario, or terminal — the mechanism is
designed to be extended along those three axes, and ad-hoc additions
break the cross-terminal / time-series guarantees.

---

## 1. The matrix

Three axes: **scenario × terminal × metric**.

### 1.1 Scenarios (the rows)

Each scenario is a reproducible workload. Files live in
`bench/scenarios/`; drivers live in `bin/scenarios/<id>.sh`. New
scenarios get a `bench/scenarios/<id>.spec.md` with the rationale.

| ID | Workload | What it stresses | Cross-terminal? |
|---|---|---|---|
| `cat-ascii` | `cat 32 MiB ASCII` | raw byte throughput, 1B/cell | yes |
| `cat-mixed` | `cat 16 MiB ANSI-coloured log` | parser CSI/SGR + render | yes |
| `cat-cjk` | `cat 8 MiB CJK` | wide-char path + font fallback | yes |
| `cat-emoji` | `cat 8 MiB emoji` | non-BMP + colour glyph (SBIX/COLR) | yes |
| `vim-jump` | open 50 k-line file, jump to bottom + back | escape density, scroll, cursor | yes (manual) |
| `htop-60s` | `htop` for 60 s, full repaint per second | sustained periodic repaint | yes |
| `scroll-10k` | print 10 k lines, wheel-scroll back | scrollback access + render | partial — wheel events not scriptable cross-term |
| `scrollback-1m` | push 1 M lines, sample random-access scroll | large history index + disk spill | yes for fill+RSS+disk; marspot-only for jump latency |
| `multi-session-9x` | 9 sessions, each running a mid-load workload | aggregate throughput, total RSS, per-window FPS | yes for RSS+wall-time; marspot-only for FPS |
| `idle-9x` | 9 sessions opened, then idle 5 min (or 30 m extended) | CPU at rest, RSS drift, disk drift | yes |
| `session-switch` | sidebar / tab click between 9 active sessions | TTFF (time-to-first-frame) | marspot-only (UI-internal) |
| `typing-latency` | scripted keystrokes, measure input → pixel | input latency tail | marspot-only (instrumented); cross-term via screen capture is a future item |
| `startup` | spawn terminal cold → first prompt | startup cost + first-paint | yes |

The first 4 (`cat-*`) are the legacy throughput row. The middle group
adds the **realistic interactive surface**. The last 5 are
**Marspot-as-a-product** scenarios — they exist because the project's
no.1 value is multi-session / unlimited-scroll, and a bench that
doesn't measure those doesn't measure the product.

### 1.2 Terminals (the columns)

| Terminal | Role | Driver |
|---|---|---|
| `marspot`  | the subject under test | direct (`MARSPOT_SHELL` one-shot script, `--bench` modes, env-var instrumentation) |
| `iterm2`   | competitor | AppleScript → `tell application "iTerm" … write text` |
| `warp`     | competitor | AppleScript → `open -a Warp`, System Events keystroke |
| `ghostty`  | competitor | binary `-e <wrapper-script>` (Ghostty has no AS dict; no keystroke needed) |
| `terminal` | OS-vendor reference floor | AppleScript → `tell application "Terminal" … do script` |

`competitors_snapshot` updates of `iterm2` / `warp` / `ghostty` all
participate in the `vs-best-other` floor — `bin/bench.sh` iterates the
snapshot rather than hardcoding two terminals, so adding a competitor
is a baseline-only edit. Terminal.app is excluded from the floor by
design (OS-vendor reference, not a competitor we measure against).

### Snapshot schema (per terminal)

Each entry under `competitors_snapshot` self-describes when and against
what build it was measured, so a drifting snapshot (one terminal
refreshed today, another stuck on an older date) is auditable at a
glance:

```json
"ghostty": {
  "version":     "1.3.1 (build 15212)",
  "bundle_id":   "com.mitchellh.ghostty",
  "captured_at": "2026-06-07",
  "method":      "ssh→LaunchAgent (com.marspot.bench-trigger)",
  "cat-ascii_MBps":  78.0,
  "cat-mixed_MBps":  84.2,
  "cat-cjk_MBps":   114.3,
  "cat-emoji_MBps": 100.0
}
```

`competitors_snapshot.host` records the bench host (model, OS, arch)
all four entries were measured on. `competitors_snapshot.captured_at`
(top-level) tracks the most-recent per-entry refresh. The
`vs-best-other` iteration skips non-MBps entries (`host`, `_comment`)
automatically via `if key in v`.

`bin/_remote-measure-others-mini.sh` probes
`/Applications/<App>.app/Contents/Info.plist` for version + bundle_id
on each run so the metadata is filled in automatically — no manual
edit needed when refreshing.

Cross-terminal driving uses a single contract: each driver puts a
fresh window in a known state, executes the scenario, and writes
`/tmp/measure-<terminal>-<scenario>.txt` with `/usr/bin/time -p`
output and an `==ALL_DONE==` sentinel. The harness polls for the
sentinel; absolute paths defeat shell aliasing. Manual paste is the
fallback when a driver flakes (see `bin/measure-other.sh`).

### 1.3 Metrics (the cells)

For every (scenario, terminal) cell we collect a subset of:

| Metric | Unit | Cross-terminal | How |
|---|---|---|---|
| **throughput**       | MB/s | ✓ | `time -p cat <scn>`; bytes / real |
| **wall-time**        | s    | ✓ | `time -p` real |
| **rss-active**       | MiB  | ✓ | `ps -o rss=` while scenario runs |
| **rss-idle-after**   | MiB  | ✓ | `ps -o rss=` 5 s after scenario completes |
| **cpu-active-avg**   | %    | ✓ | `ps -o %cpu=` polled every 250 ms |
| **disk-bytes**       | bytes | ✓ | sum of files in the terminal's scrollback dir (marspot: `~/.cache/marspot/scrollback`; iterm2: `~/Library/Application Support/iTerm2/SavedState`; warp: SQLite db; terminal.app: 0 — no disk persistence) |
| **fps-p50/p95**      | hz   | marspot-only | renderer counter via `MARSPOT_PROFILE` |
| **input-latency-p50/p95** | ms | marspot-only | keystroke→setContents via `MARSPOT_LATENCY` |
| **ttff-switch**      | ms   | marspot-only | sidebar click → first frame committed |
| **startup-cold**     | ms   | ✓ | spawn → window-visible (osascript polling) |
| **binary-size**      | bytes | marspot-only | `stat -f%z target/release/{marspot,mcli}` |

Cells without a measurement record `null`, not 0. Skipping is loud
(`- skip` in the gate output) so a silently-missing metric can't fool
a passing run.

---

## 2. What's comparable and what isn't

We do not pretend to compare what isn't comparable.

- **Throughput / wall-time / RSS / CPU / disk / startup**: cross-term
  apples-to-apples. Use the same scenario file, the same shell
  invocation, the same machine, the same display. Numbers fold into
  `bench/results/cross-terminal.json` and the comparison table in
  `docs/perf.md`.

- **FPS, input-latency, TTFF**: instrumented from inside marspot, no
  way to measure the same thing in iTerm2 / Warp / Terminal.app
  without invasive screen-capture or hardware-camera setups. We
  track them longitudinally for **marspot vs marspot** regression. A
  rough cross-term "screen-capture latency" track is a future
  scenario; for now we declare these marspot-only.

- **Multi-session aggregate FPS**: meaningless cross-term — iTerm2 /
  Warp redraw via different mechanisms (Cocoa retained mode vs
  Electron). We compare aggregate **throughput** + total **RSS** +
  total **CPU** instead, which are the user-visible cost of running
  9 sessions.

---

## 3. Driver design

Drivers live in `bin/scenarios/<id>.sh` (one file per scenario). Each
driver implements a small, fixed contract:

```sh
# bin/scenarios/<id>.sh <terminal> <out-json>
#   <terminal>  one of marspot|iterm2|warp|terminal
#   <out-json>  path to write per-(terminal,scenario) result JSON
#
# Exit 0 = ran successfully; emits JSON with at least:
#   { "scenario": "<id>", "terminal": "<term>", "metrics": { … }, "skipped": [..] }
# Exit 2 = scenario not applicable to this terminal (cleanly skipped)
# Exit non-zero = real failure
```

The top-level `bin/bench.sh --full --terminal=<list>` walks the matrix:
for each (scenario, terminal) where the scenario doesn't declare
`unsupported`, it dispatches the driver and merges its JSON into
`bench/results/<runid>.json`.

### Terminal-specific dispatch

- **marspot**: pointed at a one-shot `MARSPOT_SHELL` script. Profiling
  env-vars (`MARSPOT_PROFILE=…`, `MARSPOT_LATENCY=…`) get forwarded so
  per-trial counters land alongside throughput.
- **iterm2**: `osascript` creating a fresh window with the default
  profile, `write text` for the command, polling for marker.
- **warp**: `open -a Warp` for a fresh window, `osascript`
  System-Events keystrokes for the command. Brittle — `--print-only`
  paste fallback when it misfires.
- **ghostty**: binary `-e <wrapper-script>`. Ghostty 1.3.x exposes no
  meaningful AppleScript dictionary and `--command=` is single-binary
  only, so the driver writes the shell command to a temp script and
  passes its path to `-e`. The surface spawns the wrapper, the wrapper
  writes the marker, the shell exits — no keystroke, no AS.
- **terminal.app**: `osascript`, `do script`. Most stable of the
  three; useful as a sanity check when iterm2/warp drivers misbehave.

### Why per-scenario driver scripts (not one big switch)

Three reasons: (1) `multi-session-9x` and `scrollback-1m` need very
different per-terminal logic — they spawn N tabs / push synthetic
events / watch many markers, and trying to share that with the simple
`cat` flow leads to a 500-line `case` statement. (2) Per-file scripts
are individually runnable for debugging (`bin/scenarios/scrollback-1m.sh
marspot /tmp/foo.json`). (3) When iTerm2 changes its AppleScript surface
(this happens), one driver file rots, not the whole bench.

---

## 4. Output and persistence

Two layers, intentionally separate:

### 4.1 Per-run snapshot (`bench/results/<runid>.json`)

`<runid>` = `YYYYMMDD-HHMMSS-<short-sha>`. Self-describing; full
matrix in one file. Schema:

```json
{
  "run_id": "20260504-141233-abc1234",
  "started_at": "2026-05-04T14:12:33Z",
  "git_sha": "abc1234…",
  "git_dirty": false,
  "machine": { "model": "MacBookPro18,2", "cpu": "Apple M1 Max", "ram_gb": 32, "macos": "14.4" },
  "scenarios": {
    "cat-ascii": {
      "marspot":     { "metrics": { "throughput_MBps_p50": 92.1, "rss_active_MiB": 41, … } },
      "iterm2":   { "metrics": { "throughput_MBps_p50": 56.4, … } },
      "warp":     { "metrics": { "throughput_MBps_p50": 47.3, … } },
      "terminal": { "metrics": { "throughput_MBps_p50": 38.0, … } }
    },
    …
  }
}
```

The most recent snapshot becomes
`bench/results/cross-terminal.json` (a symlink — kept for the gate's
backwards compatibility).

### 4.2 Time-series (`bench/results/timeseries.jsonl`)

Append one line per (run_id, scenario, terminal, metric_set). This is
what answers "did we get faster / slower over the last 30 commits?"
without parsing 30 snapshots. Committed; trimmed only when it crosses
a couple-of-MB.

### 4.3 The Markdown view (`docs/perf.md`)

Auto-regenerated from the most recent snapshot whenever someone runs
`bin/bench.sh --regen-perf-md`. Numbers stay in JSON (the truth);
prose stays in Markdown (the narrative). Don't hand-edit numbers in
`docs/perf.md` — they will be overwritten and mis-trusted.

---

## 5. Gate rules

The gate in `bin/bench.sh` reads `bench/baseline.json` (floors and
ceilings, with safety margin baked in) and refuses to merge a PR that
breaks any of:

| Class | Rule |
|---|---|
| **Floors** (higher = better) | `parse-MBps`, `live-MBps`, `vs-best-other-ratio` ≥ floor |
| **Ceilings** (lower = better) | `render-p99-µs`, `binary-size`, `idle-RSS` ≤ ceiling |
| **Tail** | `input-latency-p99` ≤ ceiling (marspot-only) |
| **Drift** | `idle-9x`: RSS at t=5 min ≤ RSS at t=30 s × 1.10 (no creep) |
| **Multi-session** | `multi-session-9x` aggregate throughput ≥ floor; total RSS ≤ ceiling |

A floor / ceiling is intentionally tighter than the worst observed
sample — the `--update-baseline` workflow bakes 7 % (headless) /
10 % (live) margin so identical code doesn't flap.

The **vs-best-other-ratio** is the literal "outperform iTerm2/Warp"
check. If the ratio falls below 1.0× on a scenario where it used to
be > 1.0×, the gate fails and the PR description has to acknowledge it.

### Tier split (fast vs --full)

The gate is split across two tiers so pre-push runs stay deterministic
on a loaded dev box and the noisy-but-load-bearing checks only fire
where they can be trusted:

| Check class | fast (every commit / pre-push) | `--full` (pre-merge / clean machine) |
|---|---|---|
| `parse-MBps` (headless 5-trial median) | ✓ | ✓ |
| `render-p99-µs`, `scroll-p99-µs` (headless) | ✓ | ✓ |
| binary size, idle RSS | ✓ | ✓ |
| `live-MBps` (cat-ascii / mixed / cjk / emoji) | — | ✓ |
| `vs-best cat-*` ratio (vs competitors_snapshot) | — | ✓ |
| `multi-session-9x` throughput + vs-best | — | ✓ |
| `scrollback-1m` throughput + vs-best | — | ✓ |

**Why the split.**  multi-session-9x and scrollback-1m are
single-trial AppleScript-driven measurements with ±10–20 % thermal /
foreground-load variance on the dev box.  Floors are locked
clean-machine (≥3-trial median, M1 Max, post-godot-cleanup; see
`baseline.json._comment`) with only 10 % safety margin, so they will
flap on a busy dev box even when nothing regressed.  Gating them at
fast tier would break the "fast bench is deterministic" property the
pre-push hook depends on.

**Where `--full` should run.**  `bin/bench-remote.sh --full`
dispatches it to `ssh mini` (clean idle M4 / 64 GB).  Running
`--full` locally on a loaded dev box is allowed but expected to flap
on multi-session; the warning is `bench-remote.sh` exists for a
reason.

### `live-MBps` source: absorption rate, not parse rate (perf-attack E8)

`live cat-*` compares marspot to competitors, so it MUST use the same
metric they're measured in: the `time cat` **absorption rate** (how fast
`cat` finishes writing to the PTY — fast, because the pipeline buffers and
cat doesn't fully block; the grid parses async behind it).  `load_live`
uses `competitors_snapshot.marspot` (the absorption rate, co-measured with
competitors), falling back to `bin/measure.sh`'s standalone-mcli `live.json`.

Do **not** confuse this with the L3 **parse** rate (`bin/measure-l3.sh` /
`l3_throughput` probe — when the grid actually finishes ingesting, ~0.90×
the in-process parse rate, ~0.4× the absorption rate).  That's a real
internal-health number — `--full` prints it as an ungated informational
line — but it is NOT the cat-* gate metric (gating absorption-rate
competitors against marspot's parse rate is apples-to-oranges; that
mistake was made + reverted 2026-06-14).  Absorption is architecture-
independent, so the snapshot floors hold across the L3 flip; re-capturing
the absorption number on the L3 app (vs the pre-L3 snapshot) is a step-6
refinement.

---

## 6. How to extend

**Add a scenario**: drop `bin/scenarios/<id>.sh`, add a
`bench/scenarios/<id>.spec.md`, append a row to the matrix in this
doc, add a baseline floor/ceiling entry in `bench/baseline.json`,
re-run `bin/bench.sh --update-baseline`.

**Add a terminal**: drop `bin/drivers/<term>.sh` implementing the
launcher contract (single function: `run_in_<term> <cmd> <marker>`),
register it in `bin/measure-other.sh`'s `OTHER_TERMINALS` list and in
`bin/_remote-measure-others-mini.sh`'s `TERMS` list, run the latter to
capture a competitor snapshot, paste the numbers into
`bench/baseline.json` `competitors_snapshot`. No edits to `bin/bench.sh`
— the `vs-best-other` gate iterates the snapshot automatically.

**Add a metric**: it must be defined for every cell where it could
apply (or explicitly marked unsupported). Add it to the metrics table
above; add capture code to the relevant driver(s); add a gate rule
in `bench/baseline.json` if it's load-bearing for the value
proposition.

**Don't**: add a one-off script outside this mechanism. If you need
a one-off ad-hoc measurement (which happens — e.g. profiling a
specific bug), do it under `bench/adhoc/<date>-<topic>/` and don't
let it leak into the matrix.

---

## 7. Remote bench host (clean idle Apple Silicon)

The local dev box is a poor place to lock in a baseline — foreground
apps, browser tabs, IDE language servers, and the bench harness
itself contend for the same cores.  A 5–10 % thermal / scheduler tail
on the dev box is normal and would otherwise force margin-wide
baselines that mask real regressions.

`bin/bench-remote.sh` dispatches the same gate (`bin/bench.sh` or
`bin/bench.sh --full`) on a dedicated quiet host:

- Default `HOST=mini` (`doracawl@mini` — M4 / 64 GB / macOS 26.5.1,
  same Apple Silicon arch as the studio).  Override with
  `HOST=<ssh-alias> bin/bench-remote.sh`.
- Independent working tree at `~/bench-marspot/` with its own
  `CARGO_TARGET_DIR`, so building on the remote host doesn't fight
  with local builds for the cargo lock or target/.
- Lock + cleanup contract: refuses to start if a previous run is
  still holding the lock; cleans the lock + working state on exit
  (success or failure).
- Results land in `bench/remote-runs/<UTC-iso>/` with the full
  stdout + exit code preserved for inspection.

Use `bin/bench-remote.sh --full` before any merge that intends to
update `bench/baseline.json`, and any time the local dev box is
under load while you'd otherwise have to choose between flaky
numbers and waiting.

### Cross-terminal numbers on the remote host

ssh sessions on macOS **cannot dispatch AppleEvents to GUI apps**
(by design — AppleEvents need an authenticated GUI session).
What survives ssh varies by terminal and was mapped empirically:

| Terminal | ssh-driven? | Bridge |
|---|---|---|
| `ghostty` | **yes** | `sudo -n launchctl asuser <uid> ghostty -e <wrapper>` — injects spawn into the user's GUI launchd domain so NSApp + Metal bootstrap. Surface runs as root, but throughput numbers are PTY-bound and unaffected by uid. |
| `iterm2`  | no | `bsexec gui/<uid>` lets osascript find iTerm by name but `tell application "iTerm" to count windows` returns -1728 even when iTerm is running; `sudo asuser` osascript hits a TCC Automation grant the console user has to authorize once. AS syntax also varies between iTerm versions (3.6.11 rejects `create window with profile "Default"` at parse time). |
| `warp`    | no | Driver depends on `tell application "System Events" to keystroke`. SE keystroke is TCC-Accessibility-gated and unreachable from any ssh-spawned osascript regardless of bridge (-1712 timeout). |
| `terminal`| no | Same SE / Automation gating as warp/iterm2. |

One-time NOPASSWD sudoers entry on the bench host enables the asuser
bridge:

```
echo "doracawl ALL=(root) NOPASSWD: /bin/launchctl asuser $(id -u) *" \
  | sudo tee /etc/sudoers.d/marspot-bench
sudo chmod 0440 /etc/sudoers.d/marspot-bench
```

`bin/_remote-measure-others-mini.sh` auto-detects SSH_CONNECTION and
runs only Ghostty in ssh mode; iTerm/Warp/Terminal snapshots are
preserved untouched by per-terminal merge in
`bin/remote-measure-others.sh`. To refresh those, Screen Share into
the bench host and invoke the script with no SSH_CONNECTION set.

When the snapshot can't be refreshed right now (remote host is
headless, you're in a hurry), set
`MARSPOT_BENCH_ALLOW_STALE_COMPETITORS=1` to bypass the freshness
check with a stamped warning.  The gate still runs against the
existing `competitors_snapshot` numbers; the warning is the audit
trail.

### Toolchain alignment

`bin/sync-toolchain.sh` (default `HOST=mini`) one-shots the
13 cargo bins the bench / gate scripts depend on (`cargo-nextest`,
`cargo-fuzz`, `cargo-deny`, `cargo-audit`, `cargo-machete`,
`samply`, etc.) to the same versions on the remote host as on the
dev box.  Run after upgrading any of those locally, or as the
first step of bringing up a new bench host.

---

## 8. Why this matters more than the cat-* numbers alone

The 4 `cat-*` scenarios were sufficient when the goal was "be fast on
a stream of bytes." The product goal is bigger: **be the terminal
that holds 9 Claude-Code sessions for a full working day, each with
hundreds of thousands of lines of history, and never get slower or
heavier over the day.** That goal collapses if RSS creeps, if disk
fills, if FPS drops when sessions multiply, or if scrollback access
becomes O(n). The matrix above is calibrated to surface each of
those failure modes.
