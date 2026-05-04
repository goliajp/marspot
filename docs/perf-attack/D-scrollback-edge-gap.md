# D — scrollback dramatic-edge gap

> Status: queued
> Master:  ../perf-attack.md
> Related: B (post-parse pipeline), C (RSS bloat)

## What's broken

`scrollback-1m` pushes 1,000,000 lines (~80 MiB) into the terminal,
measuring how fast it ingests.  This is a flagship mars scenario —
the whole "unlimited scroll" architecture (anon-mmap ring, line
indexer, MADV_SEQUENTIAL) was built for it.  Numbers from
2026-05-05:

| ID | mars | iTerm2 | Terminal.app | mars edge |
|---|---|---|---|---|
| D1 | 79.3 MB/s | – | 79.1 MB/s | **1.003×** ⚠️ basically tied |
| D2 | 79.3 MB/s | 73.1 MB/s | – | 1.08× ⚠️ thin |

Terminal.app has *no scrollback persistence at all* — it just runs
text through the renderer.  mars doing the full anon-mmap ring
write + index + render + scrollback structure should be either
*faster* (if the architecture pays for itself) or *clearly slower
with a structural reason* (if write throughput is bounded by mmap
dirty-page rate).  Tied is the worst result: the architecture
isn't paying off, AND mars is doing more work.

The push throughput should be ≥ 1.5× both competitors for the
"dramatic perf advantage" framing — current state is "noise gap."

## Why it matters

`scrollback-1m` is the closest analog to the multi-hour
Claude-Code session use-case (lots of output, history matters).
If mars can't beat Terminal.app on the simplest variant (1 session,
1M lines), the multi-session case is at risk too.  Plus: the
unlimited-scroll value prop is sold on this number.

## TDD failing tests

### D1: scrollback-1m vs Terminal.app

```sh
bin/scenarios/scrollback-1m.sh mars     /tmp/d-mars.json
bin/scenarios/scrollback-1m.sh terminal /tmp/d-term.json
# Currently: 79.3 / 79.1 = 1.003×
# Target:   mars/Term ≥ 1.5× (mars ≥ ~119 MB/s if Term holds 79)
```

### D2: scrollback-1m vs iTerm2

```sh
bin/scenarios/scrollback-1m.sh iterm    /tmp/d-iterm.json
# Currently: 79.3 / 73.1 = 1.08×
# Target:   mars/iTerm2 ≥ 1.5× (mars ≥ ~110 MB/s if iTerm2 holds 73)
```

Combined exit: mars ≥ 110 MB/s on push throughput (clears both
floors comfortably).

## Hypotheses

1. **Push pipeline serial bottleneck** — the test pushes 1M lines
   via cat over PTY.  Possible the bottleneck is *not* the
   scrollback architecture but the per-line: PTY read → parser
   → grid append → scrollback append flow being serial / blocking.
   Profile to find the dominant step.
2. **mmap dirty-page rate caps writes** — anon mmap ring is
   page-aligned; writing across pages dirties them; OS page-out
   pressure on a 16 GB Mac with mars loaded could cap us around 80
   MB/s on the ring path.  Verify: same scenario with
   `MARS_DISK_SCROLLBACK=0` (memory mode) — if numbers jump, the
   mmap path is the cap.  If not, we're upstream-bottlenecked.
3. **Renderer wakeup cost dominates** — every batch of lines
   wakes the renderer for a frame; if the wakeup cost is high,
   1M lines = many small frames.  Frame coalescing (already
   implemented?) would help.  Verify via Instruments.
4. **Parser is per-byte loop with branches** — 1M lines of
   plain text could be hammering the parser even though the
   "headless parse" gate passes.  Live includes more checks
   (cursor position update, dirty-rect accumulation).

## Investigation roadmap

1. **Memory-mode A/B**: run `MARS_DISK_SCROLLBACK=0
   bin/scenarios/scrollback-1m.sh mars` 3× and compare to
   default (disk-backed).  Quantifies hypothesis 2.
2. **Time-profile push** — Instruments while running scrollback-1m.
   Categorise: PTY syscalls, parser, grid append, ring write,
   render submit.  Identify > 30% bucket.
3. **mcli vs mars (1×1) comparison** — if mcli is faster than
   mars 1-cell, the multi-session machinery is paying tax on
   single-session.  If they match, machinery is fine.
4. **MADV_SEQUENTIAL effectiveness** — confirm via vmstat /
   `sysctl vm.swapusage` that pages are being released as
   advised.  If retention is high, advice is being ignored.

## Implementation roadmap

By root cause:

**If mmap-write-bound (hypothesis 2):**
- Larger writes per syscall (batch line-writes into 4 KiB chunks
  before write())
- Pre-fault the next ring page on penultimate write of current
- Consider `O_DIRECT`-style bypass — but only after verifying
  the dirty-page accounting is the actual cap

**If parser-bound (hypothesis 4):**
- SIMD-ish bulk path for "no escape sequence" runs (memcpy-fast)
- Batch grid-write per line instead of per-byte cursor advance
- Profile-guided inlining of hot parser branches

**If renderer-wakeup bound (hypothesis 3):**
- Coalesce frames during high-throughput push (drop intermediate
  frames; only render the latest)
- Already in place via dirty-flag rendering — verify firing
  correctly under flood

**If grid-append bound:**
- Per-line grid append instead of per-byte cursor advance
- Skip cursor-advance for output that wraps anyway

## Exit criteria

1. D1: mars ≥ 110 MB/s (≥ 1.5× Terminal.app's 79.1)
2. D2: same ≥ 110 satisfies vs iTerm2 too
3. mars per-cell (1×1) push ≥ 110 MB/s — confirms machinery
   isn't taxing single-session
4. RSS during 1M-line push stays ≤ 200 MiB total (no unbounded
   growth on flood)
5. `bench/baseline.json` updates `multi_session_thresholds.scrollback-1m`
   floor to reflect new locked-in number

## Risks

- **Don't trade scrollback architecture for raw push speed** —
  the disk-backed ring exists because RAM-only ring can't hold
  10M lines × N sessions.  If push speed comes from disabling
  scrollback by default, that's a feature regression.
- **Frame coalescing during push must not affect typing latency**
  — after the push ends, the renderer must be at p95 latency
  the next frame.  Coalescing decision logic must be tied to
  burst detection, not always-on.

## Progress log

- 2026-05-05 — item filed from bench-run 20260505-055755-6889ffb
