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
- **B4 (emoji) is real (~14× ascii)** — the cost is **Apple Color Emoji
  sbix bitmap decode/scale inside `draw_glyphs`**.  Per-phase breakdown
  (2026-06-14, `MARSPOT_GLYPH_PROFILE` timing inside `rasterise_glyph`):

  | phase | ascii/cjk | emoji |
  |---|---|---|
  | `get_bounding_rects` | ~0 µs | ~19 µs |
  | `CGBitmapContextCreate` + buffer | ~0 µs | ~2 µs |
  | `set_*` properties | ~0 µs | ~0 µs |
  | **`draw_glyphs`** | **~9 µs** | **~146 µs** |

  This **definitively kills the roadmap's pooling tactic**: context
  creation + properties + alloc are ~0 µs for every class — there is
  nothing to pool.  Even the shared baseline is `draw_glyphs` (9 µs), not
  setup.  Emoji's 146 µs is CoreText decoding + scaling the embedded sbix
  PNG — inherent to drawing Apple Color Emoji; the only way around it is
  reimplementing sbix decode (huge, wrong self-build/perf tradeoff vs the
  CoreText FFI we already depend on).  It's paid **once per unique emoji,
  then cached**; emoji churn is rare; under L3 raster is decoupled from cat
  drain (frame latency under churn, not cat MiB/s).  **Verdict: accept the
  perf cost — no sound optimisation exists.**

  The investigation surfaced the *actual* emoji gap, which is correctness
  not perf: **marspot renders emoji MONOCHROME.**  The atlas is `R8Unorm`
  (alpha-only) and the FG shader outputs `float4(fg.rgb, fg.a * coverage)`
  — so an emoji is the cell's text colour tinted by the emoji's alpha
  silhouette, never its real colours.  Colour emoji would need a separate
  RGBA atlas + a second FG shader path (sample RGBA directly).  That's a
  feature with real scope, and a product decision (is colour emoji a v1
  goal?) — not tracked as a B4 perf item.

| ID | Metric | Status |
|---|---|---|
| B1 | live cat-ascii | **resolved 2026-05-05** (was godot artifact) |
| B2 | live cat-mixed | **resolved 2026-05-05** (was godot artifact) |
| B3 | live cat-cjk | **retracted 2026-06-13** — cjk raster = ascii raster (`--bench glyphraster`); not a raster problem, L3 parse drains it fine (E8). |
| B4 | live cat-emoji | **accepted (perf) 2026-06-14** — 146 µs of the 169 µs is CoreText sbix decode in `draw_glyphs` (ctx/props/alloc ≈ 0 → pooling tactic dead); inherent, once-per-unique-emoji + cached + rare + decoupled under L3. No sound perf win. Surfaced the real gap: **emoji render monochrome** (R8 atlas) — a colour-emoji *feature* (RGBA atlas + shader), product decision, not B4-perf. |

### C — Per-session RSS bloat vs Terminal.app — **re-scoped onto L3 2026-06-13**

C was filed against the **standalone** marspot (one process: 9 in-process
grids + Metal + a per-cell scrollback ring + the 16 MiB atlas) and shares
A1's root (lazy-fault-into-mmap-ring).  A1 was resolved under L3.
Re-measured the way the product runs (`bin/soak-l3-rss-scaling.sh`, probe
`l3_rss_scaling`: N idle `marspot-session` processes, dev box):

| N | total idle RSS | per-session |
|---|---|---|
| 1 | 1.94 MiB | 1.94 MiB |
| 3 | 5.73 MiB | 1.91 MiB |
| 9 | 17.2 MiB | **1.91 MiB** |

**Per-session idle L3 RSS = ~1.9 MiB, perfectly linear** (each session is
an independent ~1.9 MiB process; no fixed bloat, no superlinear creep).
The core (1 shared renderer + 9 synthetic grid mirrors) is ~27 MiB
*fixed* regardless of pane count; the shell owns the single window.  So
the marginal cost per added session is ~1.9 MiB.

| ID | Scenario | standalone Δ | under L3 | File | Status |
|---|---|---|---|---|---|
| C1 | idle-9x first sample | +90 MiB | **9 × 1.91 = 17.2 MiB total** (≤ 30 MiB target ✓) | [C](perf-attack/C-per-session-rss-bloat.md) | **RESOLVED 2026-06-13** (like A1) — gated by `soak-l3-rss-scaling.sh` |
| C2 | vim-jump post | +93 MiB | per-session stays ~idle (light-active) | [C](perf-attack/C-per-session-rss-bloat.md) | **resolved** (same root as C1; full vs-Term needs live) |
| C3 | htop-60s mean | +93 MiB | per-session stays ~idle | [C](perf-attack/C-per-session-rss-bloat.md) | **resolved** (same) |
| C4 | active-9x-soak max | +228 MiB | scrollback-ring working set (bounded, disk-backed/evictable, ~same total as 9 standalone rings, no leak) | [C](perf-attack/C-per-session-rss-bloat.md) | **re-scoped → D** (ring-resident sizing, not a bloat leak) |

**Takeaway**: under L3 the "10×–80× more per-session" framing no longer
holds — idle is a flat ~1.9 MiB/session process-isolation cost (the
crash-isolation design the user approved), not unbounded bloat.  C1/C2/C3
resolve with A1.  C4's active footprint is the scrollback ring's resident
working set — intrinsic to "9 sessions each with live scrollback", bounded
and disk-backed; the knob is ring-resident sizing, tracked under D.  Full
vs-Terminal.app absolute numbers (shell window + Term via GUI) need a live
measurement, folded into the per-session-L3 step-6 live pass.

### D — scrollback dramatic-edge gap — **re-scoped 2026-06-14**

Measuring D (`marspot --bench scrollaccess`, `bin/soak-scrollback-access.sh`)
surfaced two facts that re-frame it:

1. **Scrollback is a fixed ~26 624-line ring**, not "unlimited / 1M / 10M":
   `DISK_SCROLLBACK_RAM_LINES (1024) + DISK_SCROLLBACK_PAGES (100) ×
   LINES_PER_PAGE (256) = 26 624` lines, ~50 MiB anonymous mmap, bounded
   forever. Feeding 2 M lines retains the most recent 26 624. The "access
   at 1M depth" target was based on a ring size that doesn't exist by
   default. (`cell_at_view`'s `view_offset: u16` caps viewport *scroll* at
   65 535, but the 26 624 ring cap binds first; the O(1) data primitive is
   `scrollback_cell(idx: usize, …)`.)
2. **Within the ring, access is O(1)**: cold (post-MADV_DONTNEED) viewport
   read latency is **flat across depth** — 1916–4166 ns at depths [0, 1k,
   10k, 26623], deepest *fastest* (deep/shallow 0.46×), RSS bounded at
   ~52 MiB. An O(N) ring at 26 k depth would be milliseconds+.

So the honest dramatic edge isn't a push-throughput multiplier (D1/D2 are
a thin ~1.07× over Term — its no-persistence renderer is hard to beat by
1.5× when marspot also writes the ring) — it's **O(1) cold access across
the full retained ring** while Term/iTerm lose history beyond their
buffer. Now gated by `bin/soak-scrollback-access.sh`.

| ID | Metric | Status |
|---|---|---|
| D1 | scrollback-1m vs Term push | **accept 2026-06-14** — ~1.07× (97.7 / 91.6); thin but ahead. Not a 1.5× "dramatic" multiplier; push throughput isn't where the edge lives. Floor unchanged. |
| D2 | scrollback-1m vs iTerm2 push | **accept** — ~1.36× (97.7 / 71.8); ahead, not chasing higher. |
| D3 | **O(1) cold scrollback access at depth** (new) | **done 2026-06-14** — flat cold read 1916–4166 ns across the full 26 624-line ring, bounded RSS; gated by `soak-scrollback-access.sh`. This is D's real structural edge. |

**Open product decision (not a bug):** the retained depth is 26 624 lines
by default. Growing it (bigger `DISK_SCROLLBACK_PAGES`) trades RSS/swap for
deeper history — a config knob for the user, not chased here.

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
| E3 | vim-jump cross-term wall-time capture | [E](perf-attack/E-bench-infra.md) | **satisfied 2026-06-14** — vim-jump.sh rewrite already captures wall-time for marspot/iterm/terminal (shared `time -p` worker); only Warp left out (paste-mode driver can't close windows / verify). |
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
   bitmap decode** in `draw_glyphs` (~146 of 169 µs; ctx/props/alloc ≈ 0,
   so the pooling micro-opt is dead — nothing to pool).  **B4-perf
   accepted, no sound win** (inherent CoreText decode, once-per-unique +
   cached + rare + decoupled under L3).  No remaining glyph *perf* work.
   The real emoji gap is correctness: **monochrome rendering** (R8 atlas) —
   colour emoji is an RGBA-atlas *feature*, a product decision, not a
   perf-attack item.
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
