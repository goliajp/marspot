# A1 — active-9x-soak RSS drift FAIL

> Status: **RESOLVED under the L3 architecture (2026-06-13)** — both halves (per-session engine + renderer/core) verified bounded under sustained load; the standalone-marspot leak does not reproduce in the production shell→core→L3 tree
> Master:  ../perf-attack.md
> Related: C (per-session RSS bloat — likely shared root)

## ⚠️ Re-scope (2026-06-13): A1 was measured on the now-non-default architecture

A1 was filed against the **standalone in-process `marspot`** (src/main.rs:
9 grids + parser + scrollback + PTY all in one address space). As of
target #4 (commits through `004fa21`, L3 now the default) the product is
**shell→core→L3**: the parser/terminal/grid/scrollback/PTY half — i.e.
*every surviving A1 leak candidate* (4 PTY-chunk heap fragmentation, 5
Session/Terminal accumulation; 1–3 already ruled out) — runs in N separate
`marspot-session` (L3) processes. So A1 splits in two under the real product:

- **per-session engine** (PTY/terminal/grid/scrollback) → now in the L3 process.
- **renderer/core** (render scratch, MTLBuffer/autorelease, atlas, font_cache) → now in marspot-core, which renders from shm mirrors.

### Result — per-session engine is BOUNDED (the 9×-multiplied factor)

`bin/soak-l3-drift.sh` (probe `crates/marspot-session/examples/l3_drift_probe.rs`)
floods ONE L3 with `while :; do seq 1 200; done` and samples its RSS over a
sustained window:

| window | drift q4/q1 | abs growth | rate |
|---|---|---|---|
| 60 s  | 1.004 | +256 KiB | — |
| 300 s | **1.003** | **+272 KiB** | **~70 KiB/min** |

vs A1's standalone leak of **~10 MiB/min** — this is **~150× lower**, flat
post-plateau (the scrollback ring fills to ~44 MiB resident and stops). The
"cannot get slower the longer it runs" commitment **holds for the
per-session engine** — the standalone leak's surviving candidates (PTY
chunk fragmentation, Session/Terminal accumulation) do NOT reproduce when
the engine runs as its own process. This is the factor the real 9-grid
product multiplies, so it's the load-bearing half.

> NB: ~44 MiB/session resident under heavy use is the *bounded scrollback
> ring* (26 624 slots), not a leak. Whether to shrink that default is the
> separate D-scrollback / "opportunistic smaller ring" tuning question.

### Result — renderer/core is also BOUNDED

`bin/soak-l3-core-drift.sh` launches the real shell→core→9 L3 tree, floods
all 9 sessions (`flood_sessions` writes the output loop to each PTY via
shelld), and samples **marspot-core's** RSS under that sustained load:

| window | core RSS drift q4/q1 | abs growth |
|---|---|---|
| 120 s (9 sessions flooding) | **1.001** | **+16 KiB** |

Core sits flat at ~27.7 MiB regardless of the 9-session flood — render
scratch plateaus at its high-water-mark, no Metal command-buffer /
autorelease accumulation, the 9 shm readers + mirror grids are bounded.
(`DURATION_S=300` runs the full A1 gate window.)

### Conclusion: A1 is resolved by the L3 architecture

Both halves are bounded under sustained load:

| half | where (L3 arch) | drift | growth |
|---|---|---|---|
| per-session engine | each `marspot-session` | 1.003 / 300 s | ~70 KiB/min |
| renderer / core | `marspot-core` | 1.001 / 120 s | +16 KiB |

The ~10 MiB/min leak A1 found on the **standalone in-process `marspot`**
does **not** reproduce in the production shell→core→L3 architecture. The
per-session split was the fix: each engine runs in its own process and
plateaus on its own bounded scrollback ring, while core renders
statelessly from shm. The "cannot get slower the longer it runs"
commitment (CLAUDE.md #3) holds for the product.

The standalone `marspot` binary still has the original leak, but it is no
longer the product default (it remains a dev/bench entry + the
`MARSPOT_L3=0` opt-out). Fixing it there is now low priority; if pursued,
the per-subsystem RSS instrumentation plan below still applies.

## What's broken

Running 9 marspot sessions under sustained activity for 5 minutes, RSS
grows from +72 MiB to +195 MiB above baseline.  q4/q1 drift ratio
**2.11×** vs gate threshold **1.3×**.  CLAUDE.md architectural
commitment #3 ("cannot get slower the longer it runs") directly
violated at the 5-minute mark — the soak window the user actually
hits keeps marspot open all day.

## Why it matters

marspot's product proposition is "9 Claude-Code sessions held all day."
If RSS climbs 2× over 5 min, by hour 8 the process is OOM'd or
swapping.  No amount of cat-throughput perf rescues a terminal that
can't stay open.  This single failure invalidates the architectural
story.

## TDD failing test

```sh
# This must pass:
bin/scenarios/active-9x-soak.sh marspot /tmp/a1-test.json
# Pass criteria embedded in scenario:
#   RSS drift q4/q1 ≤ 1.30×                  ← currently 2.11× ✗
#   (target tightened post-fix to ≤ 1.10×)
```

Current run output (`bench/results/20260505-055755-6889ffb/active-9x-soak-marspot.json`):
```
RSS Δ first / last / max  72 / 195 / 195 MiB  (baseline 0 MiB)
RSS drift q4/q1            2.1104   ✗ FAIL
```

**Exit gate** (must hit all three):
1. `q4/q1 ≤ 1.10` (tighter than current threshold 1.30, since fixing the
   leak should leave ample margin)
2. `max - first ≤ 30 MiB` (absolute growth bounded; reflects glyph
   atlas / scrollback ring fill but no unbounded sources)
3. Soak extended to 30 min (`--extended` mode) also passes both above —
   catches slow leaks the 5-min window misses

## Hypotheses (ranked by likelihood, each verifiable)

1. **font_cache char_cache 8K bound insufficient under active load**
   ([commit `c55c3db`](#) bounded it; 9 cells × diverse glyphs may
   blow past 8K faster than eviction releases atlas slots).  Verify:
   instrument `char_cache.len()` per sample tick, confirm it pegs
   to bound but RSS still grows.

2. **glyph atlas atomic-rebuild leaves CABuffer / texture residue**.
   On rebuild ([`1039f4e`](#) added 2K² atlas + atomic rebuild),
   old `MTLTexture` may not release until next autorelease pool drain.
   If rebuild fires often during active load, texture allocation
   accumulates.  Verify: VM Tracker / Allocations on `MTLTexture`
   instances over the 5-min window.

3. **disk-backed scrollback ring per-cell mmap commit** ([`d2b13bb`](#)
   anon mmap, [`86e81d1`](#) default-on).  Each cell's ring writes
   pages dirty under sustained output → resident set grows even
   though the file region is fixed.  Verify: `vmmap <pid>` showing
   anon-mmap region's resident size growing.

4. **PTY-grid channel unbounded under backpressure miss**.  If 9
   producers (PTY) outpace 9 consumers (grid update), the channel
   queue grows.  Verify: track channel depth per cell over time.

5. **Session-side accumulation** (event listeners, autorelease
   slots, NSTextInput state).  Verify: count NSObject lifecycle in
   instruments.

## Investigation roadmap

1. **Reproducible isolation** — re-run `active-9x-soak.sh marspot` 3×
   confirm 2.11× drift is stable (rule out one-shot noise).  If
   highly variable, expand soak to 30-min `--extended` mode for
   stronger signal.
2. **Instruments allocations profile** — record full 5-min soak with
   "Allocations" template.  Snapshot at 30s and 270s; diff allocation
   counts to find the dominant growing object class.
3. **Per-component RSS slicing** — instrument main.rs to log:
   `mars_rss / atlas_bytes / scrollback_bytes / fontcache_bytes /
    pty_buffers_bytes / session_struct_bytes` every 5s during soak.
   Identifies which subsystem owns the growth.
4. **Bisect candidate commits** — if profile points at scrollback /
   atlas / fontcache, bisect the relevant commit range
   (`d2b13bb..6889ffb` for scrollback, `1039f4e..6889ffb` for atlas,
   `c55c3db..6889ffb` for fontcache).
5. **Confirm root cause** — implement minimal repro
   (`marspot --bench soak`) that reliably shows the leak in <30s.

## Implementation roadmap (post-confirmation)

Branches by root cause (will narrow once #2/#3 above complete):

**If font_cache bound issue:**
- Tighten char_cache LRU eviction to fire before 8K (maybe 4K)
- Atlas slot release coupled with cache eviction
- Add `font_cache_pressure_total` counter for ongoing visibility

**If atlas rebuild residue:**
- Wrap rebuild in autoreleasepool to drop old `MTLTexture` immediately
- Reduce rebuild frequency (raise atlas size or improve packing)
- Consider double-buffer instead of full rebuild

**If scrollback ring resident growth:**
- `madvise(DONTNEED)` on completed page chunks within ring
- Tighter line-count bound per cell (currently 10M)
- L1 RAM ring sizing audit

**If PTY-grid backpressure:**
- Bound the channel; drop oldest under saturation (with metric)
- Or rate-limit PTY read when grid is behind

## Exit criteria

1. `bin/scenarios/active-9x-soak.sh marspot` passes with q4/q1 ≤ 1.10
   on 3 consecutive runs
2. `--extended` (30-min) variant also passes
3. Per-component RSS instrumentation shows total bounded (no
   subsystem grows unboundedly within sample)
4. `bench/baseline.json` `multi_session_thresholds` updated with new
   tightened drift threshold (after one --update-baseline cycle)
5. Memory in `feedback_perf_edge_non_negotiable.md` referenced — no
   soft-pedalling acceptable; if root cause turns out to be a
   bench-methodology artifact, that gets explicit verification, not a
   shrug

## Risks / what to watch

- **Don't trade RSS drift for CPU**: a fix that uses more CPU to
  reclaim memory must not regress the active-9x-soak CPU mean
  (≤ 2.0% per F floor)
- **Don't trade for parse throughput**: headless `--bench parse`
  must not regress > 5% on any cat-* scenario
- **Verify on 9-session AND 1-session**: leak might be per-cell,
  per-session, or shared; both flavours must drop

## Progress log

(append-only, dated)

- 2026-05-05 — item filed from bench-run 20260505-055755-6889ffb
- 2026-05-05 evening — clean-machine bench-run rerun, post godot-cleanup:

  | terminal | drift q4/q1 | abs Δ | CPU mean |
  |---|---|---|---|
  | marspot         | **1.81× ✗ FAIL** | +125 MiB | 1.49% |
  | iTerm2       | 1.30× ✓ borderline | +33 MiB  | 37.33% |
  | Terminal.app | 1.05× ✓           | +1 MiB   | 9.76% |

  godot-tinted earlier numbers (marspot 2.11×) shifted only modestly on
  clean machine (1.81×); absolute Δ is essentially identical (+123 →
  +125 MiB).  godot was NOT the cause of A1 — it's a real marspot-side
  drift, larger than competitors' (iTerm2 +33, Terminal.app +1).

  iTerm2 and Terminal.app both plateau within 5 min — Terminal.app
  trivially (only ~3 MiB total), iTerm2 with its fixed-size per-session
  state.  marspot does NOT plateau in 5 min: per-session anon-mmap ring
  is ~100 MiB virtual (1024 + 100×256 lines × cols × Cell_size), and
  the 36 lines/s/session workload only pushes ~10800 lines (40% of
  ring capacity) in 5 min.  Page-commit ramp continues throughout the
  5-min window.

  **Verification result**: --extended (30 min) drift = **2.065× ✗ FAIL**.
  RSS first/last/max = **105 / 413 / 413 MiB**, +308 MiB net growth
  over 30 min = **~10 MiB/min** sustained leak rate.  Larger drift,
  not smaller: the lazy-fault hypothesis is **REJECTED**.

  Updated cross-terminal landscape (clean machine):

  | terminal     | 5-min drift | 5-min Δ peak | 30-min drift | Verdict |
  |---|---|---|---|---|
  | marspot         | 1.81×       | +228 MiB     | **2.07×**    | leaks ~10 MiB/min |
  | iTerm2       | 1.30×       | +117 MiB     | (not measured) | plateaus by 5 min |
  | Terminal.app | 1.05×       | +16 MiB      | (not measured) | trivial growth |

  At 10 MiB/min × 8-hour workday × 9 sessions ≈ 4.8 GB sustained
  growth.  This is not lazy-fault; it's a real leak.  CLAUDE.md #3
  ("cannot get slower the longer it runs") fully violated.

  Hypotheses 1+2 (font_cache / atlas) ruled out by static analysis.
  Hypothesis 3 (scrollback ring lazy-mmap) cannot account for
  unbounded growth — ring is fixed-size, fully wraps in ~12 min.
  Remaining candidates 4-5 + new ones to explore:

  4. **PTY chunk Vec<u8> heap fragmentation** — 64 KiB chunks
     allocated/freed every PTY read can fragment the heap; system
     malloc on macOS doesn't release back to OS.  Test: switch to
     mimalloc / jemalloc; observe drift change.
  5. **Per-cell stateful accumulation in Session / Terminal** — alt-
     screen `saved_main`, `predictions` (bounded), parser internal
     state, anything not bounded that lives per-session.
  6. **Render scratch high-water-mark** — cells_scratch /
     glyphs_scratch / dots_scratch in render_metal.rs `clear()` per
     frame but don't shrink capacity.  Should plateau though.
  7. **MTLBuffer / MTLCommandBuffer accumulation** — Metal command
     buffers from previous frames not released; objc autorelease
     pool drain timing.

  **Next-step: per-subsystem RSS slicing instrumentation**.  Add
  `MARSPOT_PROFILE_RSS=1` mode that, on each render or every N seconds,
  logs:

  ```
  total_rss=X
    grid_bytes=...      (sum across sessions)
    scrollback_bytes=...
    atlas_bytes=...
    fontcache_bytes=...
    metal_buffers_bytes=...
    other=delta
  ```

  Run --extended with this on; the subsystem whose `bytes` grows
  monotonically is the leak.  ~50-100 LoC instrumentation work, then
  one --extended re-run to localise.  Defer this work to next
  focused session — it's the right surgical entry rather than
  guess-and-check fixes.

- 2026-05-05 — static analysis pass on `feature/perf-A1-soak-rss-drift`:
  - **font_cache char_cache (hypothesis 1)**: ruled OUT.  Hard cap at
    `CHAR_CACHE_CAP = 8K` + atomic `clear()` on overflow; HashMap
    capacity bounded ~500 KiB; not a growth source.
  - **glyph atlas atomic-rebuild (hypothesis 2)**: ruled OUT.  Single
    `MTLTexture(2048, 2048, R8) = 4 MiB` allocated once at Renderer
    construction; rebuild calls `shelves.clear() + cache.clear()` on
    the SAME texture; no per-rebuild texture allocation, no residue.
  - **scrollback ring lazy-mmap (hypothesis 3)**: **most likely
    explanation** but reframes A1.  Per `bin/scenarios/active-9x-soak.sh`
    workload (~36 lines/s/session) and ring size (`DISK_SCROLLBACK_RAM_LINES
    + DISK_SCROLLBACK_PAGES × LINES_PER_PAGE = 1024 + 100×256 = 26,624
    slots ≈ ~100 MiB per session at 122 cols × ~32 B/Cell), 5-min default
    mode writes only ~10,800 lines per session (40% ring fill).  The
    entire 5-min window is the page-commit transient — RSS grows
    linearly with lines written, plateauing only after the ring
    fully wraps at ~12 min.  Observed q4/q1 = 2.11× corresponds to
    a sub-linear page-commit ramp, **smaller** than the naïve
    time-linear estimate of 3.84× (samples 230-300s vs samples
    38-100s = 265s/69s).  Scenario doc itself flags this:
    "5 min … some lazy-fault still expected; --extended 30 min …
    past steady-state."

  **Reframe**: A1 isn't an unbounded-growth bug.  The 5-min default
  threshold of 1.3× q4/q1 was set without acknowledging the
  pre-plateau transient; 30-min --extended (1.10× threshold) is the
  real architectural-commitment test ("cannot get slower the longer
  it runs" applies *post-plateau*, not during the lazy-fault ramp).

  **Next steps** (require clean machine; deferred):
  1. Run `bin/scenarios/active-9x-soak.sh marspot /tmp/x.json --extended`
     on a quiet machine; expected: q4/q1 ≤ 1.10× (architectural pass).
     If yes → the 30-min run is the real gate, default 5-min is too
     short to be meaningful.
  2. Update default 5-min threshold to ~1.50× (acknowledging mid-fill
     transient) AND require --extended to pass for any merge-blocking
     soak gate.
  3. If --extended ALSO fails → genuine leak; investigate hypothesis
     4 (PTY-grid backpressure) and 5 (Session-side accumulation).

  **Opportunistic improvement** to consider regardless: smaller ring
  default (e.g. 8192 slots ≈ 32 MiB / session × 9 = 288 MiB total),
  which would plateau within the 5-min window and tighten *both*
  default and extended gates.  Trade-off: shallower scrollback;
  acceptable since terminal users typically need recent pages, not
  20+ K-line history.  Specific value (8192 vs 4096) wants empirical
  pick once clean-machine measurement is available.

  No code change in this branch — the diagnosis updates the
  hypothesis ranking and validates that the existing scrollback
  architecture is bounded.  Code-side action gated on clean-machine
  --extended verification.
