# The first frame after a core boot — decomposition

**Status**: Phase A complete (read-only + instrumentation + synthetic
measurement).  **No attack has been implemented**, deliberately: the
Pre-Phase-B gate wants the target verified against the *real* workload,
and the instrument that produces those numbers only just shipped.

## Why this exists

2026-08-11/12, on the installed app: the shell declared the core hung
and SIGKILLed it four times in five minutes, tripping the crash budget
and freezing the window behind a banner.  Two log lines placed the
core's own main loop at the scene:

    13:18:31Z  l2.loop.stall  main loop iteration took 41.14s  slowest=render
    22:02:01Z  l2.loop.stalling  phase=attach-render   … then nothing, SIGKILL 15 s later

"render took 41 s" names no cost that can be attacked.  Neither does
"attach-render never finished".  The watchdog side of this incident is
fixed separately (L1 0.7.112/0.7.113); this document is about the
frame itself.

## The instrument

Three timers inside `render_layout_to_texture`, bracketing calls the
compiler cannot reorder across, plus two witnesses:

| field | what it covers |
|---|---|
| `build_us` | `build_instances` — CPU: grid walk, instance push, **and every glyph the atlas has never seen** |
| `encode_us` | command-buffer construction + all render passes + canvases |
| `gpu_wait_us` | `commit()` → `waitUntilCompleted()` |
| `glyphs_rasterised` | atlas misses this frame (both atlases) |
| `evictions` / `rebuilds` | shelf evictions / whole-atlas rebuilds *within this frame* |

Every `l2.loop.stall` report now carries the split, so the next stall
on any machine arrives already decomposed.

`--bench first-frame:<panes>[:ascii|mixed|cjk]` renders the incident's
geometry (3840×2130, 13 panes, 86×63) **with no warm-up** and reports
the cold frame beside the next warm one.  No prior bench in this repo
covered this: `bench_metal_render` renders five throwaway frames first,
precisely so the atlas is hot before the stopwatch starts.  That is
correct for a steady-state gate and is why the expensive frame had
never been measured.

## Measured — idle mini, 13 panes, 86×63, 3840×2130

| charset | cold total | build | encode | **gpu wait** | glyphs | evict | rebuild |
|---|---:|---:|---:|---:|---:|---:|---:|
| ascii | 13.0 ms | 5.3 | 0.7 | **6.7** | 85 | 0 | 0 |
| mixed | 122.2 ms | 115.1 | 0.7 | **6.4** | 8,661 | 0 | 0 |
| cjk | 536.5 ms | 529.3 | 0.7 | **6.4** | 17,642 | 0 | 0 |

Warm frame, all three: **~1.8–2.4 ms** (build ≈ 0.04 ms, glyphs 0).

### What this settles

1. **The GPU is not the bottleneck.**  `gpu_wait` is 6.4–6.7 ms and
   flat across a 40× swing in frame cost.  The leading hypothesis
   going in — that L2's `waitUntilCompleted` and L1's `nextDrawable`
   were both parked on a backed-up display pipeline — is **refuted**.
2. **The cold frame is 7–300× the warm frame, and 96–99 % of it is
   `build_instances`**, i.e. first-sight glyph rasterisation.
3. **Encoding is noise** — 0.7 ms regardless.
4. **The atlas did not thrash.**  `evict`/`rebuild` are 0 even at
   17,642 distinct glyphs, so the "4096² can't hold it, so it churns"
   story is refuted too.  It was an inference; the counter disagreed.

### Per-glyph cost, and a caveat about it

`--bench glyphraster:3000` (rasterise + upload, no frame around it):

| class | ns/glyph |
|---|---:|
| ascii | 15,880 |
| cjk | 10,637 |
| **emoji** | **632,806** |

Emoji cost **60×** a CJK glyph.  Panes running `claude` are full of
them.

The first-frame rows imply 13.3 µs/glyph (mixed) and 30.0 µs (cjk),
against `glyphraster`'s 10.6 µs.  The mixed figure agrees; the cjk one
does not, and **it has not been explained**.  A plausible mechanism —
CJK cells are double-width, so bigger bitmaps to rasterise and upload
— is a guess, not a measurement.  Do not quote 30 µs as a per-glyph
cost until something has measured it.

### One instrument failure, recorded because it looked like a finding

The first version of this bench walked `0x4E00 + pane × 4096`
unbounded.  By pane 4 it had left CJK Unified Ideographs entirely and
was drawing musical symbols and emoji — at 633 µs each.  It reported
4,541 ms / 160 µs per glyph for "cjk", a number 5× the corrected one,
and it looked exactly like a result.  Bounding the walk to the block
brought it to 536 ms.  *A measuring device that silently changes what
it measures is indistinguishable from data.*

## What is NOT established

The synthetic worst cases here are 8,661 and 17,642 distinct glyphs.
A real screen of code and prose holds a few hundred to a couple of
thousand — call it 10–120 ms cold on an idle machine.  **That does not
explain 41 s.**

The rest of the gap is machine load, and there is direct evidence for
that from the other side of the socket: with the new supervisor
witness in place, L1's own tick was measured **100.8 s late** during
this incident, plus routine 5–7 s gaps.  A machine that can deschedule
the shell for 100 seconds can stretch a 100 ms frame a long way.  What
has *not* been done is measuring the stretch factor on the core's side
under that load.

## Pre-Phase-B gate — not yet passed

Per `.claude/rules/perf-attack.md` §11, an attack needs its target
verified at double-digit pp of the *real* workload's cost.  `build`
being 96–99 % of a *synthetic* cold frame does not discharge that.
The instrumented core reports `render_split` on every stall, so the
next real stall on the user's machine supplies:

- how many glyphs a real screen actually rasterises cold, and
- whether `build` still dominates when the machine is the one that is
  loaded.

**Collect that before implementing anything below.**

## Attack candidates (ranked, unimplemented)

1. **Don't pay it on the critical frame.**  The cold frame is the one
   that must finish before the shell's PONG deadline, on every crash
   restart, hang verdict, silent update and window attach.  Warming
   the atlas off the main loop — or on a worker before the attach ack
   — moves the cost off the path that has a watchdog pointed at it.
   Blast radius: attach path only.
2. **Batch the uploads.**  Every miss is believed to be its own
   `replaceRegion` upload; one staging buffer per frame would collapse
   thousands of small uploads into one.  *Believed* — read
   `commit_raster` and measure before costing this.
3. **Emoji at 633 µs.**  60× a CJK glyph, on panes that are full of
   them.  Worth its own decomposition; may be a colour-bitmap path
   doing something avoidable.
4. **Survive the atlas across a core restart.**  Every replacement
   core starts cold, which is why three consecutive cores hit the same
   wall 15 s apart.  Largest change, largest payoff, most risk.

Candidate 1 is the one that turns a 40× frame into a non-event without
making any of it faster, which is usually the sign that the others are
optimisations and this one is the fix.
