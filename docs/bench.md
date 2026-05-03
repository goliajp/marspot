# Mars bench mechanism

This is the **persistent benchmark system** that validates Mars's
first-principles value: ultra-high-performance unlimited scrolling
across many concurrent terminal sessions, used as a working surface for
multi-session Claude-Code development.

It must answer, on demand and over time:

1. Is mars faster than iTerm2 / Warp on every realistic use-case the
   user actually hits? (Terminal.app is the OS-vendor reference floor.)
2. Does mars hold those numbers steady — across releases, across hours
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
| `scrollback-1m` | push 1 M lines, sample random-access scroll | large history index + disk spill | yes for fill+RSS+disk; mars-only for jump latency |
| `multi-session-9x` | 9 sessions, each running a mid-load workload | aggregate throughput, total RSS, per-window FPS | yes for RSS+wall-time; mars-only for FPS |
| `idle-9x` | 9 sessions opened, then idle 5 min (or 30 m extended) | CPU at rest, RSS drift, disk drift | yes |
| `session-switch` | sidebar / tab click between 9 active sessions | TTFF (time-to-first-frame) | mars-only (UI-internal) |
| `typing-latency` | scripted keystrokes, measure input → pixel | input latency tail | mars-only (instrumented); cross-term via screen capture is a future item |
| `startup` | spawn terminal cold → first prompt | startup cost + first-paint | yes |

The first 4 (`cat-*`) are the legacy throughput row. The middle group
adds the **realistic interactive surface**. The last 5 are
**Mars-as-a-product** scenarios — they exist because the project's
no.1 value is multi-session / unlimited-scroll, and a bench that
doesn't measure those doesn't measure the product.

### 1.2 Terminals (the columns)

| Terminal | Role | Driver |
|---|---|---|
| `mars`     | the subject under test | direct (`MARS_SHELL` one-shot script, `--bench` modes, env-var instrumentation) |
| `iterm2`   | primary competitor | AppleScript → `tell application "iTerm" … write text` |
| `warp`     | primary competitor | AppleScript → `open -a Warp`, System Events keystroke |
| `terminal` | OS-vendor reference floor | AppleScript → `tell application "Terminal" … do script` |

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
| **disk-bytes**       | bytes | ✓ | sum of files in the terminal's scrollback dir (mars: `~/.cache/mars/scrollback`; iterm2: `~/Library/Application Support/iTerm2/SavedState`; warp: SQLite db; terminal.app: 0 — no disk persistence) |
| **fps-p50/p95**      | hz   | mars-only | renderer counter via `MARS_PROFILE` |
| **input-latency-p50/p95** | ms | mars-only | keystroke→setContents via `MARS_LATENCY` |
| **ttff-switch**      | ms   | mars-only | sidebar click → first frame committed |
| **startup-cold**     | ms   | ✓ | spawn → window-visible (osascript polling) |
| **binary-size**      | bytes | mars-only | `stat -f%z target/release/{mars,mcli}` |

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

- **FPS, input-latency, TTFF**: instrumented from inside mars, no
  way to measure the same thing in iTerm2 / Warp / Terminal.app
  without invasive screen-capture or hardware-camera setups. We
  track them longitudinally for **mars vs mars** regression. A
  rough cross-term "screen-capture latency" track is a future
  scenario; for now we declare these mars-only.

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
#   <terminal>  one of mars|iterm2|warp|terminal
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

- **mars**: pointed at a one-shot `MARS_SHELL` script. Profiling
  env-vars (`MARS_PROFILE=…`, `MARS_LATENCY=…`) get forwarded so
  per-trial counters land alongside throughput.
- **iterm2**: `osascript` creating a fresh window with the default
  profile, `write text` for the command, polling for marker.
- **warp**: `open -a Warp` for a fresh window, `osascript`
  System-Events keystrokes for the command. Brittle — `--print-only`
  paste fallback when it misfires.
- **terminal.app**: `osascript`, `do script`. Most stable of the
  three; useful as a sanity check when iterm2/warp drivers misbehave.

### Why per-scenario driver scripts (not one big switch)

Three reasons: (1) `multi-session-9x` and `scrollback-1m` need very
different per-terminal logic — they spawn N tabs / push synthetic
events / watch many markers, and trying to share that with the simple
`cat` flow leads to a 500-line `case` statement. (2) Per-file scripts
are individually runnable for debugging (`bin/scenarios/scrollback-1m.sh
mars /tmp/foo.json`). (3) When iTerm2 changes its AppleScript surface
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
      "mars":     { "metrics": { "throughput_MBps_p50": 92.1, "rss_active_MiB": 41, … } },
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
| **Tail** | `input-latency-p99` ≤ ceiling (mars-only) |
| **Drift** | `idle-9x`: RSS at t=5 min ≤ RSS at t=30 s × 1.10 (no creep) |
| **Multi-session** | `multi-session-9x` aggregate throughput ≥ floor; total RSS ≤ ceiling |

A floor / ceiling is intentionally tighter than the worst observed
sample — the `--update-baseline` workflow bakes 7 % (headless) /
10 % (live) margin so identical code doesn't flap.

The **vs-best-other-ratio** is the literal "outperform iTerm2/Warp"
check. If the ratio falls below 1.0× on a scenario where it used to
be > 1.0×, the gate fails and the PR description has to acknowledge it.

---

## 6. How to extend

**Add a scenario**: drop `bin/scenarios/<id>.sh`, add a
`bench/scenarios/<id>.spec.md`, append a row to the matrix in this
doc, add a baseline floor/ceiling entry in `bench/baseline.json`,
re-run `bin/bench.sh --update-baseline`.

**Add a terminal**: drop `bin/drivers/<term>.sh` implementing the
launcher contract (single function: `run_in_<term> <cmd> <marker>`),
register it in `bin/measure-other.sh`'s `OTHER_TERMINALS` list, run
`bin/measure-other.sh` to capture a competitor snapshot, paste the
numbers into `bench/baseline.json` `competitors_snapshot`.

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

## 7. Why this matters more than the cat-* numbers alone

The 4 `cat-*` scenarios were sufficient when the goal was "be fast on
a stream of bytes." The product goal is bigger: **be the terminal
that holds 9 Claude-Code sessions for a full working day, each with
hundreds of thousands of lines of history, and never get slower or
heavier over the day.** That goal collapses if RSS creeps, if disk
fills, if FPS drops when sessions multiply, or if scrollback access
becomes O(n). The matrix above is calibrated to surface each of
those failure modes.
