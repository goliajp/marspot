# marspot perf attack — master tracker

This file is the **检验基础**: the canonical inventory of every place marspot
loses, ties, or holds only a thin lead, plus the gate-locked wins we
must not regress.  Every per-item roadmap lives under `docs/perf-attack/`
and links back here.

Performance is the first-principles project requirement (CLAUDE.md
"Performance is the architecture").  The discipline below is **TDD**:
each item names a currently-failing benchmark assertion and a numeric
target; implementation work is "done" only when the assertion passes
under the locked methodology, not when it "looks better."

## Source of truth

- Bench snapshot:    `bench/results/20260505-055755-6889ffb.json` (2026-05-05)
- Cross-term cat-*:  `bench/results/cross-terminal-other.json` (fresh single-trial)
- Active commit:     `6889ffb` (post visual-redesign)
- Baseline locked at: `4ce5778` (pre-Metal-renderer, pre-disk-scrollback) —
  88 commits + 5238 LoC behind HEAD; absolute-floor comparisons are
  cross-architecture.  vs-best-other ratios remain meaningful.

## L3 architecture throughput (2026-06-13, post per-session-L3 default)

Per-session L3 (shell→core→L3, now default) was characterised vs the old
in-process path with an apples-to-apples probe
(`crates/marspot-session/examples/pipeline_throughput.rs`: same shelld /
parser / payload / machine; measures `inproc` = attach a ShelldSession +
pump, vs `l3` = spawn marspot-session + read shm; the ratio isolates L3's
added cost). On the mini (cat 64 MiB): **L3 ≈ 0.90× the in-process bulk-cat
throughput** — a ~10 % cost from the per-pump shm-window memcpy + the IPC
hop, the accepted price of crash isolation / per-session update / bounded
memory. Latency, idle CPU, and memory (A1) are all verified fine under L3.

While measuring, found + fixed a real bug: `shelld` auto-attaches the
NEW_SESSION creator, so L2's `create_session` left L2 subscribed to every
session it allocated → shelld broadcast each DATA chunk to L2 too (dropped,
no inbox) on top of L3, doubling broadcast fan-out under load. Fixed by
detaching right after create (commit `18ee8b9`). The earlier "3× regression"
scare was pure measurement error (sentinel-in-command-echo; zsh-loop-bound
generation) — not real.

If the ~10 % ever matters: dirty-row-only publish, or skip publish when L2
is behind on reads. Not urgent.

**The gate now measures this path** (2026-06-13, E8 below). `bin/measure-l3.sh`
drives a real `marspot-session` per cat-* scenario headlessly (the
`l3_throughput` probe: spawn session → cat the scenario over the control
socket → time the drain via the shm scroll_push_count) and writes
`bench/results/l3-throughput.json`. `bin/bench.sh --full`'s `load_live` now
prefers that production number over the standalone-mcli `live.json` and the
hand-captured `competitors_snapshot.marspot`. **Floors are still calibrated to
the pre-L3 in-process numbers** (152–160 MiB/s); they must be re-locked
against the L3 path on the idle mini (`bin/measure-l3.sh` on the mini →
`bin/bench-remote.sh --full --update-baseline`) — until then `--full` shows
`live` / `vs-best` FAILs by design (the old gate passed only because it gated
a number ~10 % higher than the product ships). This folds into the per-
session-L3 step-6 bench re-lock.

## Items

Each row is **one independent attack project**.  Open the linked file
to see the failing test, hypotheses, roadmap, and exit criteria.
Status legend: `queued` / `active` / `blocked` / `done`.

### A — Architectural commitment violations (existential)

| ID | Title | Current (clean) | Target | File | Status |
|---|---|---|---|---|---|
| A1 | active-9x-soak RSS drift — real leak (standalone marspot) | standalone 30-min drift **2.07×** = 10 MiB/min | drift ≤ 1.10× | [A1](perf-attack/A1-soak-rss-drift.md) | **RESOLVED under L3 (2026-06-13)** — product is shell→core→L3 now; per-session 1.003/300 s (`soak-l3-drift.sh`) + core 1.001/120 s (`soak-l3-core-drift.sh`) both bounded, leak doesn't reproduce. Standalone binary still leaks but isn't default (low pri). |
| A2 | CPU drift gate appears mis-keyed | not actually a bug — gate works as designed; godot's 2.40× ✓ was the q1<1% short-circuit firing on a low-noise q1 (intentional behavior) | optional tightening for absolute-spread check | [A2](perf-attack/A2-cpu-drift-gate-bug.md) | **retracted 2026-05-05** (not a bug); optional improvement deferred |

### B — Live cat-* single-cell — **B1/B2 retracted as measurement artifact, B3/B4 still active**

2026-05-05 evening: clean-machine remeasure (godot processes killed)
showed all cat-* live numbers were ~30% slower under godot CPU
contention.  B1 and B2 originally read 51.6 / 39.0 (vs Term 1.21× /
1.10×); on clean machine they read **71.1 / 51.6** (vs Term **1.49× /
1.23×**) — comfortably above the original floors and ahead of all
competitors.  No regression.

B3 (cat-cjk) and B4 (cat-emoji) were first read as marspot losing on
Apple's CoreText/CJK and Apple Color Emoji paths.  **Root-cause measured
2026-06-13** (`marspot --bench glyphraster:N`, cache-miss rasterisation
isolated from GPU draw + parse, dev box, 3-run stable):

| script | ns/glyph | glyphs/s |
|---|---|---|
| ascii | ~15,600 | ~64k |
| **cjk** | **~14,800** | **~68k** |
| **emoji** | **~208,000** | **~4.7k** |

This **refutes the shared "glyph-atlas context-creation" root cause** the
roadmap assumed:

- **B3 (cjk) is NOT a rasterisation problem** — CJK per-glyph raster cost
  equals ascii (~15 µs).  The old "cat-cjk losing" was the standalone
  *coupled* measurement (parse+render in one process, cat blocks on the
  slowest stage) and/or the byte-throughput optics of multibyte content;
  under L3 the parse path drains cjk at ~106 MiB/s (E8, `measure-l3.sh`),
  ahead of ascii by bytes.  No raster fix is warranted.
- **B4 (emoji) is real (~14× ascii)** but the cost is **Apple Color Emoji
  sbix bitmap decode/scale inside `draw_glyphs`**, NOT the per-glyph
  `CGBitmapContextCreate` the roadmap targeted (that overhead is the
  shared ~15 µs baseline).  Pooling the context would shave the baseline
  for *all* glyphs but barely dent emoji's 193 µs sbix tail.  Optimisation
  direction therefore shifts: a color-bitmap-specific cache/scale path, or
  accept it (emoji are rare in throughput workloads and, under L3, raster
  is decoupled from cat drain — it shows as frame latency under churn, not
  cat MiB/s).

| ID | Metric | Status |
|---|---|---|
| B1 | live cat-ascii | **resolved 2026-05-05** (was godot artifact) |
| B2 | live cat-mixed | **resolved 2026-05-05** (was godot artifact) |
| B3 | live cat-cjk | **retracted 2026-06-13** — cjk raster = ascii raster (`--bench glyphraster`); not a raster problem, L3 parse drains it fine (E8). |
| B4 | live cat-emoji | **re-scoped 2026-06-13** — real ~14× raster cost, root = Apple Color Emoji sbix decode (not context-creation). Needs a color-bitmap raster path OR accept (decoupled from cat throughput under L3). queued. |

### C — Per-session RSS bloat vs Terminal.app

Likely shares root with A1.  Tracked separately so it doesn't
disappear if A1 turns out to be a different cause.

| ID | Scenario | marspot Δ | Term Δ | File | Status |
|---|---|---|---|---|---|
| C1 | idle-9x first sample | +81 MiB | ~0 | [C](perf-attack/C-per-session-rss-bloat.md) | queued |
| C2 | vim-jump post | +61 MiB | ~0 | [C](perf-attack/C-per-session-rss-bloat.md) | queued |
| C3 | htop-60s mean | +83 MiB | +1 | [C](perf-attack/C-per-session-rss-bloat.md) | queued |
| C4 | active-9x-soak max | +195 MiB | +16 | [C](perf-attack/C-per-session-rss-bloat.md) | queued |

### D — scrollback dramatic-edge gap

Clean-machine numbers reveal the gap is smaller than first read but
still real.

| ID | Metric | Clean current | Target | File | Status |
|---|---|---|---|---|---|
| D1 | scrollback-1m vs Term push | marspot **97.7** / Term 91.6 (**1.07×**, was 1.003× godot) | ≥ 1.5× Term | [D](perf-attack/D-scrollback-edge-gap.md) | queued |
| D2 | scrollback-1m vs iTerm2 | marspot 97.7 / iTerm2 71.8 (**1.36×**, was 1.08× godot) | ≥ 1.5× iTerm2 | [D](perf-attack/D-scrollback-edge-gap.md) | queued |

### G — marspot vs Ghostty (new competitor 2026-06-06)

Ghostty 1.3.1 entered `competitors_snapshot` 2026-06-06 (mini, M4,
3-trial median). `bin/bench.sh` `vs-best-other` now iterates the
snapshot rather than hardcoding `max(iterm2, warp)`, so Ghostty's
numbers participate. cat-mixed / cat-cjk / cat-emoji fail floor;
cat-ascii stays ahead. Same underlying mechanism as B3/B4 (CoreText
glyph atlas), bundled with that attack window.

| ID | Metric | Current (vs Ghostty) | Target | File | Status |
|---|---|---|---|---|---|
| G1 | live cat-mixed | 1.50× (133.3 / 88.9)  | ≥ 1.19× | [G](perf-attack/G-vs-ghostty-cjk-emoji-mixed.md) | **retracted 2026-06-07** — bench harness was measuring competitors concurrently, not sequentially |
| G2 | live cat-cjk   | 1.17× (133.3 / 114.3) | ≥ 0.70× | [G](perf-attack/G-vs-ghostty-cjk-emoji-mixed.md) | **retracted 2026-06-07** — was contention artefact, marspot actually outperforms |
| G3 | live cat-emoji | 1.40× (160.0 / 114.3) | ≥ 0.85× | [G](perf-attack/G-vs-ghostty-cjk-emoji-mixed.md) | **retracted 2026-06-07** — same fairness bug |

### E — Bench infrastructure fixes (block honest measurement)

Must land before A/B/C/D so subsequent diagnoses aren't polluted by
noise / stale data / missing metrics.

| ID | Item | File | Status |
|---|---|---|---|
| E1 | competitors_snapshot staleness guard | [E](perf-attack/E-bench-infra.md) | **done** (2026-05-05, `feature/perf-E1-staleness-guard`) |
| E2 | fast-gate warm-up trial | [E](perf-attack/E-bench-infra.md) | **done** (2026-05-05, `feature/perf-E2-warmup`) |
| E3 | vim-jump cross-term wall-time capture | [E](perf-attack/E-bench-infra.md) | queued |
| E4 | measure-other.sh stale-marker cleanup | [E](perf-attack/E-bench-infra.md) | **done** (2026-05-05, `feature/perf-E4-E5-measure-other-hardening`) |
| E5 | measure-other.sh 3-trial median | [E](perf-attack/E-bench-infra.md) | **done** (2026-05-05, `feature/perf-E4-E5-measure-other-hardening`) |
| E6 | active-9x-soak CPU drift gate direction (= A2) | [A2](perf-attack/A2-cpu-drift-gate-bug.md) | **retracted** (not a bug — q1<1% short-circuit by design) |
| E7 | bench scripts kill marspot/mcli by name (friendly-fire) | [E](perf-attack/E-bench-infra.md) | **done** (2026-05-05, `feature/perf-F-recalibrate-clean`) |
| E8 | gate measures production shell→core→L3, not standalone mcli | [E](perf-attack/E-bench-infra.md) | **done (measurement) 2026-06-13** — `l3_throughput` probe + `bin/measure-l3.sh` + `bench.sh --full` `load_live` prefers `l3-throughput.json`. **Floor re-lock pending on idle mini** (see L3-throughput section). |

## F — Locked floors / ceilings · **recalibrated to clean-machine 2026-05-05**

`feature/perf-F-recalibrate-clean`.  Initial F-floors (`feature/perf-F-gate-lock`)
were calibrated against godot-loaded numbers (~30% slow); recalibrated
on clean machine after godot was killed.  Floors now reflect clean-
machine reality with safety margins (parse 7%, live 10%, render/scroll
50/30% thermal swing).  `bin/bench.sh` (fast tier) passes 13/13 with
moderate margin on current code.

Lesson: **always verify machine state before locking perf floors.**
godot at ~500% CPU made every cat-* measurement under-state by 30%.
The earlier "regression" diagnosis (B1/B2) was contamination, not a
real perf change.

- multi-session-9x aggregate ≥ **100 MiB/s** (current 113.2; -11% margin)
- multi-session-9x aggregate ratio vs iTerm2 ≥ **5.0×** (current 5.96×)
- multi-session-9x aggregate ratio vs Terminal.app ≥ **1.8×** (current 2.04×)
- multi-session-9x peak RSS Δ ≤ **40 MiB** (current 19; +110% margin for 9-cell variance)
- idle-9x CPU mean ≤ **0.1%** (current 0.0%)
- idle-9x RSS drift q4/q1 ≤ **1.05×** (current 1.0023×)
- htop-60s CPU mean ≤ **0.5%** (current 0.0%)
- active-9x-soak CPU mean ≤ **2.0%** (current 1.24%)
- vs iTerm2 cat-ascii live ratio ≥ **1.5×** (current 1.84×)
- vs iTerm2 cat-cjk live ratio ≥ **4.0×** (current 4.87×)
- vs iTerm2 cat-emoji live ratio ≥ **10.0×** (current 11.6×)

## Execution plan

The detailed phased plan with TDD tests, exit criteria, and rollback
paths lives in [`execution-plan.md`](perf-attack/execution-plan.md).
Summary order below.

## Recommended attack order

**Updated 2026-05-05** based on this session's findings.

Done in this session:
- E1 + E2 + E4 + E5 ✓ (bench infra hardened)
- E7 ✓ (friendly-fire fix; surfaced from --extended interaction)
- F ✓ (recalibrated against clean-machine numbers; gate 13/13 ✓)
- A2 ✗ retracted (not a bug — gate by-design short-circuit)
- B1 + B2 ✗ retracted (godot CPU contention, not a real regression)
- A1 in flight (--extended verification of lazy-fault hypothesis)

Remaining queue:

1. **A1 finish** — based on --extended outcome:
   - if drift ≤ 1.10× → confirm lazy-fault transient; relax 5-min
     gate or add fill-rate check
   - if drift > 1.10× → real leak; Instruments allocations + per-
     subsystem RSS slicing
2. **B3 retracted, B4 re-scoped, G retracted** (2026-06-13, via
   `--bench glyphraster`).  The assumed shared root cause (per-glyph
   `CGBitmapContextCreate` overhead) was wrong: cjk raster cost = ascii
   (~15 µs/glyph), so B3 is not a raster problem at all; emoji is the
   only real loss (~14× ascii) and its cost is Apple Color Emoji **sbix
   bitmap decode**, which context-pooling doesn't touch.  Remaining glyph
   work is B4-only and OPTIONAL: a color-bitmap-specific cache/scale path.
   Under L3 it's frame latency under emoji churn, not cat MiB/s (raster
   is decoupled — see L3-throughput section).  The context-pooling
   micro-opt would shave the shared ~15 µs baseline for all glyphs but is
   low-value (most glyphs are cached after first sight).
3. **C** (few days, probably folded into A1 fix) — per-session RSS
   bloat is largely the same lazy-fault footprint as A1
4. **D-rescope** (1 week) — D-target reframed: scrollback ACCESS at
   large depth (marspot: O(1) mmap fault; Term/iTerm2: cap'd, can't
   even access).  Add new scenario gating that, rather than push
   throughput.
5. **E3** (1 day) — vim-jump cross-term wall time capture (driver gap)

## How to update this file

- When an item finishes, mark `done` in the table and link to the
  commit / PR that landed the fix.
- When a new perf gap surfaces (from a fresh bench run), add it as the
  next `<bucket><n>` entry under the right bucket and create a roadmap
  file under `perf-attack/`.
- When F floors are recalibrated (intentionally), bump the value here
  AND in `bench/baseline.json`, and note it in the per-item file's
  progress log.

## Related

- `docs/bench.md` — bench mechanism (matrix, drivers, gate rules)
- `docs/perf.md` — historical perf budget + measurement narrative
- `bench/baseline.json` — machine-readable gate thresholds
- `bench/results/timeseries.jsonl` — append-only history of every run
