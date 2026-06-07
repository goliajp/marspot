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

## Items

Each row is **one independent attack project**.  Open the linked file
to see the failing test, hypotheses, roadmap, and exit criteria.
Status legend: `queued` / `active` / `blocked` / `done`.

### A — Architectural commitment violations (existential)

| ID | Title | Current (clean) | Target | File | Status |
|---|---|---|---|---|---|
| A1 | active-9x-soak RSS drift — confirmed real leak | 5-min drift 1.81× / 30-min drift **2.07× ✗ FAIL** / +308 MiB over 30 min = **10 MiB/min sustained leak** | drift ≤ 1.10× --extended | [A1](perf-attack/A1-soak-rss-drift.md) | **active — needs per-subsystem RSS instrumentation** |
| A2 | CPU drift gate appears mis-keyed | not actually a bug — gate works as designed; godot's 2.40× ✓ was the q1<1% short-circuit firing on a low-noise q1 (intentional behavior) | optional tightening for absolute-spread check | [A2](perf-attack/A2-cpu-drift-gate-bug.md) | **retracted 2026-05-05** (not a bug); optional improvement deferred |

### B — Live cat-* single-cell — **B1/B2 retracted as measurement artifact, B3/B4 still active**

2026-05-05 evening: clean-machine remeasure (godot processes killed)
showed all cat-* live numbers were ~30% slower under godot CPU
contention.  B1 and B2 originally read 51.6 / 39.0 (vs Term 1.21× /
1.10×); on clean machine they read **71.1 / 51.6** (vs Term **1.49× /
1.23×**) — comfortably above the original floors and ahead of all
competitors.  No regression.

B3 (cat-cjk) and B4 (cat-emoji) are smaller losses than first read but
still real: marspot vs Apple's CoreText/CJK and Apple Color Emoji paths.

| ID | Metric | Clean current | Target | File | Status |
|---|---|---|---|---|---|
| B1 | live cat-ascii (MB/s) | 71.1 (vs Term 1.49×, iTerm 1.27×) | ≥ 70 ✓ already | [B](perf-attack/B-live-cat-regression.md) | **resolved 2026-05-05** (was godot artifact) |
| B2 | live cat-mixed | 51.6 (vs Term 1.23×, iTerm 1.84×) | ≥ 50 ✓ already | [B](perf-attack/B-live-cat-regression.md) | **resolved 2026-05-05** (was godot artifact) |
| B3 | live cat-cjk | 36.4 (vs Term 0.86×, **vs Warp 0.77× — losing**) | ≥ 42 (1.0× Term) AND ≥ 47 (1.0× Warp) | [B](perf-attack/B-live-cat-regression.md) | queued |
| B4 | live cat-emoji | 42.1 (vs Term 0.84× — losing, vs Warp 0.95×) | ≥ 50 (1.0× Term) | [B](perf-attack/B-live-cat-regression.md) | queued |

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
2. **B3 + B4 + G1/G2/G3** (1-2 weeks) — CJK/emoji vs Apple's
   CoreText/SBIX paths, and vs Ghostty's hot glyph atlas. Same root
   cause; G-series exposed when Ghostty entered the snapshot and the
   gate stopped masking it. Specific tactic: pool CGBitmapContext +
   reuse bitmap buffer in `glyph_atlas::rasterise_glyph` (per-glyph
   context creation + property setting is ~10-30% of CJK/emoji
   raster cost). Exit when `bench-remote --full` passes 21/21 with
   Ghostty in the snapshot (current: fails 3/21 on G1/G2/G3).
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
