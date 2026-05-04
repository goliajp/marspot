# C — per-session RSS bloat vs Terminal.app

> Status: queued
> Master:  ../perf-attack.md
> Related: A1 (likely shared root); D (mmap ring sizing)

## What's broken

Across every scenario where mars and Terminal.app both run, mars
holds substantially more RSS per session.  Terminal.app is the
macOS-vendor reference floor — beating Apple's minimal terminal on
CPU is one thing, paying 10×–80× more per-session RSS is a
structural-bloat tell.

| ID | Scenario | mars Δ | Term Δ | Multiplier |
|---|---|---|---|---|
| C1 | idle-9x first sample      | +81 MiB | ~0 | n/a (Term is rounding-down) |
| C2 | vim-jump post             | +61 MiB | ~0 | n/a |
| C3 | htop-60s mean             | +83 MiB | +1 | 83× |
| C4 | active-9x-soak max        | +195 MiB| +16 | 12× |

iTerm2 numbers are between mars and Terminal.app: idle-9x +17 MiB,
htop +3 MiB, vim-jump +5 MiB, active-soak +90 MiB.  So mars is
heavier than *both* competitors on every one of these scenarios.

C4 is also the per-instant peak from A1's drift trajectory; if A1's
fix lands, C4 likely shrinks alongside.  C1-C3 may or may not share
the same root.

## Why it matters

The product proposition is "9 sessions held all day, never gets
slower."  Even if A1 fixes the *drift*, holding 81 MiB at idle for 9
sessions when Terminal.app holds ~0 means the per-cell static
footprint is heavy.  On a 16 GB machine the user keeps 9 mars +
Slack + browser + IDE — mars's static budget matters.

Also: a heavy idle baseline is a leak-magnet.  Bugs that cause +5
MiB/hour are obvious against +0 MiB baseline, invisible against +83
MiB baseline.

## TDD failing tests

### C1: idle-9x first-sample RSS

```sh
# Currently:
bin/scenarios/idle-9x.sh mars /tmp/c1.json
# RSS Δ first sample = +81 MiB
# Target: ≤ 30 MiB (still allows realistic 9 × ~3 MiB per session)
```

### C2: vim-jump post-state RSS

```sh
bin/scenarios/vim-jump.sh mars /tmp/c2.json
# RSS Δ post = +61 MiB
# Target: ≤ 25 MiB
```

### C3: htop-60s mean RSS

```sh
bin/scenarios/htop-60s.sh mars /tmp/c3.json
# RSS Δ mean = +83 MiB
# Target: ≤ 30 MiB
```

### C4: active-9x-soak max RSS

```sh
# Already covered by A1's exit gate (max - first ≤ 30 MiB).
# Here the additional check is the ABSOLUTE max ≤ 100 MiB
# (independent of drift).  Currently 195.
```

**Single combined gate after fix lands**: `bench/baseline.json`
gains `per_session_rss_max` cells per scenario; gate fires if mars's
RSS Δ exceeds.

## Hypotheses

1. **disk-backed scrollback ring per-cell anon mmap pages dirty**
   ([`d2b13bb`](#) anon mmap, [`86e81d1`](#) default-on,
   [`59cd28f`](#) ring is 10M lines).  Each cell's ring has a fixed
   reservation; resident set grows as written.  Verify: `vmmap
   <pid>` and look for anon-MALLOC regions sized ~10 MiB / cell.
2. **Glyph atlas per-process** — 2k² atlas at 4 BPP = 16 MiB.
   Lazy-allocated, but `htop-60s` and `active-9x-soak` will pay it.
   Verify: atlas RSS slice from instrumentation (see A1 step 3).
3. **Lazy-alloc backfire** — `[ed074bd]` made scrollback ring
   lazy.  If lazy means "first cell that ever scrolls allocates a
   shared ring" but each cell ends up allocating its own, the
   intended idle savings don't apply when soaking 9 cells.
4. **Layer / texture residue** — Metal layers, sublayers, or
   off-screen render targets per cell that aren't getting freed
   between draws.

## Investigation roadmap

1. **Per-component RSS breakdown** (also done for A1 — share
   instrumentation): for a 9-session idle mars, log
   `atlas_bytes / scrollback_bytes / fontcache_bytes /
    pty_buffer_bytes / mtl_layer_bytes / session_struct_bytes`,
   sum vs total RSS; the gap is leaks.
2. **Idle-1 vs idle-9 scaling** — how much of the +81 MiB is
   per-cell?  Run `mars` with 1 cell only ( `MARS_GRID=1x1` or
   similar; if not exposed, add a flag for the bench).  If
   idle-1 is +9 MiB and idle-9 is +81 MiB, scaling is linear =
   per-cell footprint dominates.  If idle-1 is also +30 MiB,
   significant fixed overhead.
3. **vmmap snapshot at idle vs mid-soak** to identify which
   region is growing
4. **Compare with mcli** (single-session) RSS for sanity — mcli
   should be ~9 MiB if the architecture is right

## Implementation roadmap

Branches by root cause:

**If scrollback ring sized too aggressive at idle:**
- Lazy-alloc the ring on first scroll, not on session creation
- Collapse 9 idle sessions to a *shared* unused ring (allocate
  per-cell only on first write)
- Cap initial allocation at 64 KiB; grow on demand

**If atlas charge:**
- Allocate atlas only on first non-trivial render (already
  tested for fonts; verify atlas is actually lazy)
- Investigate smaller atlas at idle (1k²) growing to 2k² under
  pressure — but this risks atlas-rebuild churn under load

**If lazy-alloc backfire:**
- Audit `Grid::scrollback` lazy-init logic; ensure first-write
  is the trigger, not first-construct
- Tests for "9 idle sessions = N times one idle session's RSS"

**If Metal residue:**
- Audit MTLLayer / MTLTexture lifecycle per cell; ensure cells
  share atlas / pipelines / buffers; only per-cell state is the
  command buffer for the latest frame

## Exit criteria

1. C1: idle-9x first-sample Δ ≤ 30 MiB
2. C2: vim-jump post Δ ≤ 25 MiB
3. C3: htop-60s mean Δ ≤ 30 MiB
4. C4: active-9x-soak max Δ ≤ 100 MiB (also satisfies A1's bound)
5. New gate: `mars` 1-session vs 9-session RSS scales linearly
   (Δ_9 ≤ 9 × Δ_1 + 30 MiB shared overhead)

## Risks

- **Lazy-alloc that frees too aggressively** can cost on first
  scroll (latency spike).  Don't trade idle RSS for a 50-ms first-
  scroll stall.
- **Shrinking atlas size** can trigger more rebuilds → tied to
  A1 hypothesis 2; both must move together.

## Progress log

- 2026-05-05 — item filed from bench-run 20260505-055755-6889ffb
- 2026-05-05 evening — clean-machine remeasure (godot killed):

  | scenario | mars Δ (clean) | Term Δ | iTerm2 Δ |
  |---|---|---|---|
  | idle-9x first sample | 90 MiB (was 81 godot) | 1 | 14 |
  | vim-jump post        | 93 MiB (was 61 godot) | 0 | 2 |
  | htop-60s mean        | 93 MiB (was 83 godot) | 0 | 2 |
  | active-9x-soak max   | 228 MiB (was 195 godot) | 4 | 117 |

  Notable: clean numbers are *higher* than godot-session for every C
  metric.  godot was suppressing lazy-fault commits — pages mars
  *would have* committed during normal lazy-fault didn't get touched
  because godot was monopolising CPU.  Clean machine shows the true
  page-commit ramp.

  Gap to Terminal.app remains 80-200 MiB across all scenarios.  C is
  a real per-session footprint issue, not a measurement artifact.

  Investigation tied to A1's --extended verification (in flight).  If
  A1 confirms lazy-fault-into-mmap-ring as the source, C1-C4 will
  shrink in proportion to whatever ring-sizing change lands for A1.
  C-specific work (e.g. atlas / glyph cache slicing) deferred until
  A1's data points it.
