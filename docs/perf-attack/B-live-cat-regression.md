# B — live cat-* single-cell regression

> Status: queued
> Master:  ../perf-attack.md
> Related: A1 (post-parse path likely shared root), C (per-session bloat)

## What's broken

The single-session `mcli` live PTY pipeline has degraded across all
four `cat-*` throughput scenarios since baseline lock at `4ce5778`.
On a clean post-warmup run measured on 2026-05-05:

| Sub-target | Bytes (MiB) | Live MB/s | Floor | Margin | vs Terminal | vs iTerm2 |
|---|---|---|---|---|---|---|
| **B1** cat-ascii  | 32 | 51.6 | 70 | -26.3% | 1.21× ⚠️ thin | 1.84× ✓ |
| **B2** cat-mixed  | 16 | 39.0 | 50 | -22.0% | 1.10× ⚠️ tied | 1.95× ✓ |
| **B3** cat-cjk    |  8 | 25.8 | 35 | -26.3% | **0.71× ✗** | 4.87× ✓ |
| **B4** cat-emoji  |  8 | 26.7 | 35 | -23.7% | **0.63× ✗** | 11.6× ✓ |

Two distinct sub-problems:

- **B1+B2**: post-parse pipeline has gotten slower on plain ASCII +
  ANSI text; mars is still ahead of iTerm2 but the *dramatic* edge
  has eroded.  Likely cumulative from the 88 commits / 5238 LoC
  added since baseline lock (Metal renderer arrival, disk-backed
  scrollback default-on, atlas restructures, palette changes).
- **B3+B4**: mars is *slower than the macOS reference floor* on
  wide-char and emoji.  Apple Terminal.app's CoreText + system
  Apple Color Emoji path is structurally favourable; mars's
  self-built atlas + CT raster lags.

Headless `--bench parse` passes for all four (167-211 MB/s, +4-5%
above floor) — so the regression is **strictly post-parse**: grid
update, scrollback append, Metal command encoding, draw submission.

## Why it matters

Throughput on `cat 32MB` is the most reproducible single-cell perf
demo a user runs.  When mars's "fastest terminal" claim is checked
that way, today the answer is "1.21× Terminal.app" — visible but
not dramatic, exactly the threat the perf-edge memory warns against.
On CJK / emoji mars is *slower* than the OS-vendor floor — that line
in marketing copy literally cannot be claimed today.

## TDD failing tests

### B1: live cat-ascii ≥ 70 MB/s AND vs-Terminal ratio ≥ 1.5×

```sh
# Currently fails both arms:
./bin/measure.sh cat-ascii        # mars live = 51.6, target ≥ 70
# vs Terminal.app:
# Terminal.app cat-ascii: 42.6 MB/s (cross-terminal-other.json, 2026-05-05)
# Required: 51.6 → ≥ 64 (1.5× × 42.6) — current 1.21× ratio insufficient
```

### B2: live cat-mixed ≥ 50 MB/s AND vs-Terminal ratio ≥ 1.4×

```sh
./bin/measure.sh cat-mixed        # mars live = 39.0, target ≥ 50
# Terminal.app: 35.5 MB/s; required ratio 1.4× → mars ≥ 50
```

### B3: live cat-cjk ≥ 35 MB/s AND vs-Terminal ratio ≥ 1.2×

```sh
./bin/measure.sh cat-cjk          # mars = 25.8, target ≥ 35
# Terminal.app: 36.3 MB/s; required ratio 1.2× → mars ≥ 44
# B3's stricter exit: ≥ 44 (overtake Apple's wide-char path)
```

### B4: live cat-emoji ≥ 35 MB/s AND vs-Terminal ratio ≥ 1.0×

```sh
./bin/measure.sh cat-emoji        # mars = 26.7, target ≥ 35
# Terminal.app: 42.1 MB/s; required ratio 1.0× → mars ≥ 42
# B4's stricter exit: ≥ 42 (parity with Apple Color Emoji path)
```

**Note**: all numbers above assume E5 (3-trial median) has landed.
Single-trial figures from 2026-05-05 are the seed; lock thresholds
after E5 makes measurement stable.

## Hypotheses

### B1 + B2 (ASCII / mixed regression — post-parse pipeline)

1. **Disk-backed scrollback default-on** ([commit `86e81d1`](#)) —
   per-line write to mmap'd ring on every output line.  Even with
   anon mmap ([`d2b13bb`](#)) eliminating file COW, the dirty-page
   accounting could cost on 32MB streams.  Likely contributor.
2. **Glyph atlas changes** ([`a6758e5` cell-sized slot model](#),
   [`1039f4e` 2k² + atomic rebuild](#)) — atlas writes per glyph.
   On ASCII, hot 96 glyphs should hit cache; if a regression made
   atlas miss/rewrite more, this hits.
3. **Visual redesign per-frame quad list growth** ([`6889ffb`](#)) —
   added cell title strip BG/SEAM quads + selection BG quads.  Each
   frame walks more quads.  cat 32MB triggers many frames.
4. **Renderer dispatch overhead** — Metal direct submission may
   introduce per-frame fixed cost the legacy AppKit-CGImage path
   amortised differently.

### B3 + B4 (CJK / emoji loss to Terminal.app)

1. **Per-glyph CT raster on first sight** — every wide-char / emoji
   has higher raster cost; mars caches but the *first* render of
   each unique glyph is on the hot path.  Apple's path uses shared
   system glyph cache.
2. **Emoji color glyph (SBIX/COLR) path goes through software
   blend** — if mars's atlas treats color glyphs same as monochrome,
   Apple's hardware path wins.
3. **Wide-char width calculation** — every CJK char triggers a
   `wcwidth`-style call.  If naïve, hits 100% of bytes; if Unicode
   property table is loaded lazily, first-run hit is expensive.

## Investigation roadmap

### Step 1: shared bisect (B1-B4)

Bisect commit range `4ce5778..6889ffb` (88 commits) using
`./bin/measure.sh cat-ascii` as the single signal.  Each step:
checkout, `cargo build --release`, run measure 3x, take median.
Find the commit (or commits) that caused the largest drop.

Note: bisect on 88 commits = ~7 steps × 1-2 min = 15-30 min.  Cheap.

### Step 2: per-stage time-profile (B1-B4)

Once root commit(s) found, profile that flow with Instruments
"Time Profiler" while running cat 32MB.  Categorise time:
- PTY read syscall + memcpy
- Parser CPU cost
- Grid write (cell + cursor advance)
- Scrollback append (mmap dirty)
- Atlas hit/miss
- Metal command encoding
- Frame submission

### Step 3: B3/B4-specific glyph profile

For CJK + emoji specifically, extract atlas hit/miss rates and
per-glyph raster time.  If miss rate is high on small ASCII set,
something's wrong with caching key.  If raster time dominates,
need fast-path for wide-char / color glyph.

### Step 4: confirm hypothesis with a switch

If hypothesis 1 (disk scrollback) suspected: re-run with
`MARS_DISK_SCROLLBACK=0` (memory mode).  If throughput recovers
to floor, confirmed; the disk path needs sharpening.

If hypothesis 2 (atlas changes) suspected: build with prior
atlas commit cherry-reverted, measure delta.

## Implementation roadmap

By sub-target.  Order: confirm root cause first; below is contingent.

**B1 + B2** (likely shared fix):
- If disk-scrollback regression: `madvise(WILLNEED)` on next ring
  page; chunk writes (e.g. 4 KiB batched) instead of line-granular
- If atlas: hot-path branch that skips lookup when `last_glyph_id`
  matches; per-cell glyph cache shadow
- If quad-list growth: hoist conditionals (don't push selection
  quads when no selection) — should already be the case, verify

**B3** (CJK):
- Pre-warm wide-char ranges in atlas at startup (CJK Unified
  Ideographs U+4E00..U+9FFF, ~20K glyphs is large; pre-warm on
  first CJK byte instead)
- Width-table cached, lookup once per code-point
- Consider system-shared-cache fallback for low-frequency glyphs

**B4** (emoji):
- Direct upload of SBIX/COLR bitmaps to atlas (skip CT raster step)
- Verify atlas format supports BGRA color
- Consider runtime fallback to system rendering path for emoji,
  measure if it's actually faster (Apple's path is hard to beat
  for color emoji specifically)

## Exit criteria

For each sub-target Bk:

1. `./bin/measure.sh cat-<scenario>` passes the live MB/s floor on
   3 consecutive runs (median, post-warmup)
2. Cross-terminal ratio threshold met:
   - B1: ≥ 1.5× Terminal.app (≥ ~64 MB/s)
   - B2: ≥ 1.4× Terminal.app (≥ ~50 MB/s)
   - B3: ≥ 1.2× Terminal.app (≥ ~44 MB/s)
   - B4: ≥ 1.0× Terminal.app (≥ ~42 MB/s)
3. Headless `--bench parse` for the same scenario hasn't regressed
   beyond 5%
4. multi-session-9x aggregate (the F-locked floor) hasn't dropped
5. `bench/baseline.json` floors updated with new locked values
   under `--update-baseline` workflow (post 3-trial median)

## Risks / what to watch

- **Headless parse must not be paid**: any change in the post-parse
  path that helps live throughput must not regress headless parse.
  The headless gate is the architectural ceiling — live should
  approach it, not slow parse to make live "look better".
- **Memory bound check**: B-fixes that grow caches (e.g.
  pre-warming atlas) must respect F floors on idle RSS and
  active-soak max.
- **Don't bisect during F4 instability**: E2 (warm-up) must land
  first or bisect signal is contaminated by cold-build noise.

## Progress log

- 2026-05-05 — item filed from bench-run + measure-other-fresh
