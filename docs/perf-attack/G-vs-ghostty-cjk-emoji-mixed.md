# G — marspot vs Ghostty — **RETRACTED 2026-06-07 as measurement artefact**

The G1/G2/G3 "marspot loses CJK/emoji/mixed to Ghostty" finding turned
out to be a fairness bug in the bench harness, not a real perf gap.

**Root cause**: `bin/_remote-measure-others-mini.sh` was launching all
four competitor terminals concurrently and then waiting for all four
markers. Four `cat /bench/scenarios/cat-*.bin` processes on the same
mini at the same time meant every terminal was being measured under
CPU + IO contention — and marspot was *separately* measured by
`bench-remote --full` AFTER the four competitors finished, at which
point iTerm + Terminal had been left running (the `pre=1 → leave
alone` cleanup rule), so marspot also ran under load. Different
load, different cell.

The refactor lands in this branch (`feature/add-ghostty-bench`):
sequential cycle, one terminal at a time, marspot added to the same
cycle so all five are measured under bit-identical idle conditions
with cooldowns between.

Numbers under the fair harness (2026-06-07 mini run):

| Scenario | marspot | Ghostty | ratio | floor | verdict |
|---|---:|---:|---:|---:|---|
| cat-ascii | 133.3 |  94.1 | 1.42× | 1.14× | ✓ pass |
| cat-mixed | 133.3 |  88.9 | 1.50× | 1.19× | ✓ pass |
| cat-cjk   | 133.3 | 114.3 | 1.17× | 0.70× | ✓ pass |
| cat-emoji | 160.0 | 114.3 | 1.40× | 0.85× | ✓ pass |

marspot is faster than Ghostty across every cell. The bench-remote
gate now reports **21/21 pass**. The CGBitmapContext-pooling work
this file was queuing as a P1 attack is no longer justified by
measured data — keeping the file as the retraction record so the
issue isn't re-opened from memory of the earlier (wrong) reading.

---

(original content preserved below for historical context)

# G — marspot vs Ghostty (new competitor) — cjk / emoji / mixed gap

Ghostty 1.3.1 entered `bench/baseline.json/competitors_snapshot` on
2026-06-06 (mini, M4, 3-trial median, ssh-driven via `sudo -n launchctl
asuser`). The bench gate now computes `vs-best-other` across every
recorded competitor (iterm2 / warp / ghostty), and the new floor
exposes three regressions that were previously masked by the
hardcoded `max(iterm2, warp)`:

| ID | Scenario | marspot live | Ghostty | ratio | floor | gap |
|---|---|---|---|---|---|---|
| G1 | cat-mixed  | 94.1 MB/s  | 84.2 MB/s  | 1.12× | 1.19× | marspot leads, margin under floor |
| G2 | cat-cjk    | 80.0 MB/s  | 114.3 MB/s | 0.70× | 0.70× | **at the floor** — borderline FAIL |
| G3 | cat-emoji  | 80.0 MB/s  | 100.0 MB/s | 0.80× | 0.85× | marspot 20% behind Ghostty |

Reference (still ahead): cat-ascii marspot 114.3 / Ghostty 78.0 → 1.5×.

(Numbers reflect 2026-06-07 LaunchAgent-driven refresh on mini with
iTerm/Terminal/Ghostty co-spawned under a shared GUI-session load —
slightly slower across the board than the 2026-06-06 idle-mini run
that first surfaced the gap. The relative shape is unchanged: marspot
behind on CJK + emoji, even on mixed.)

## Why Ghostty is faster on CJK / emoji

Ghostty pairs a CoreText-on-Metal render path with an aggressively
pre-warmed glyph atlas — every glyph touched in the first 100 ms ends
up resident in a Metal texture, and emoji / CJK glyphs are large
enough that the cache hit rate dominates. The result is a near-flat
throughput curve across cat-ascii / cjk / emoji (94 / 160 / 114
MB/s) — actually faster on CJK than on ASCII because the wide-char
path reduces the per-byte render call count.

marspot's current curve is the inverse: ascii fast (118), CJK / emoji
slow (80). The bottleneck is `glyph_atlas::rasterise_glyph` — per-
glyph CGBitmapContext creation + property setting accounts for
~10–30% of CJK/emoji raster cost per the B3/B4 notes. Ghostty pre-
allocates the bitmap target per surface and reuses it.

## Plan

Already noted under B3/B4 (CJK/emoji vs CoreText/SBIX) — the same
fix (bitmap-buffer pooling in `glyph_atlas::rasterise_glyph`) closes
both the B-series gap (vs Term/Warp/iTerm2) and this G-series gap
(vs Ghostty). G is not a separate attack project; it's the same
underlying issue measured against a faster opponent.

The G1 mixed-mode gap is narrower and likely a parser-side issue
(SGR escape density slowing the cat-mixed path); investigate
separately if B3/B4 fix doesn't bring G1 within floor.

## Exit criteria

- G2 ratio ≥ 0.70 (current 0.50)
- G3 ratio ≥ 0.85 (current 0.70)
- G1 ratio ≥ 1.19 (current 1.12)

All three together = `bench-remote --full` GATE PASSED 21/21 with
Ghostty in the snapshot.

## Status

`queued` — bundled with B3/B4 attack window.

## Related

- B3/B4: same underlying mechanism (CoreText glyph cache), measured
  vs Term/Warp/iTerm2 before Ghostty entered the snapshot.
- A1: long-running RSS drift — unrelated.
- `bin/bench.sh` change in this branch (2026-06-06): vs-best-other
  now iterates `competitors_snapshot` instead of hardcoding two
  terminals, so future competitors auto-participate.
