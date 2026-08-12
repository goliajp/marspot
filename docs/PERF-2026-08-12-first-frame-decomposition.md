# The first frame after a core boot — decomposition

**Status**: two attacks landed — per-frame buffer allocation
(L2 0.12.128) and scheduler priority (L1 0.7.114 / L2 0.12.131).  The
answer is at the bottom, under "What the real machine said" — and it
is not what this document's synthetic section predicted.  That section
is kept verbatim, because being wrong in a legible way is the point of
writing the prediction down first.

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


---

# What the real machine said

The instrument shipped in 0.12.125–0.12.127 and the answer arrived
within the hour.  Twelve stalls on the live app (13 panes, load ~7–10),
nine of one shape:

```
290ms  build_0.2  cmdbuf_0.0  encode_289.2 (instbuf_289.0 / 1.0MB)  canvas_0.0  gpu_1.1  glyphs_0
216ms  build_0.3  cmdbuf_0.0  encode_215.0 (instbuf_214.9 / 1.0MB)  canvas_0.0  gpu_1.1  glyphs_0
175ms  build_0.4  cmdbuf_0.0  encode_173.3 (instbuf_173.2 / 1.0MB)  canvas_0.0  gpu_1.1  glyphs_0
158ms  151ms  145ms  112ms  108ms  84ms — same shape
```

The other three were GPU-wait dominated (85–120 ms), a smaller second
mode.

**`instbuf` is 99.9 % of `encode`, and `encode` is 99 % of the frame.**
Allocating **1.0 MB** of Metal instance buffers took **83–289 ms**.
The same allocation on an idle mini is **0.09 ms for 1.6 MB** — the
one call stretches by a factor of a thousand to three thousand under
load.  `newBufferWithBytes` asks the kernel for wired memory and
registers it with the GPU driver; while it blocks, every pane is
frozen and the supervisor's PONG deadline is running.  That is the
upstream of the "please restart the app" banner.

## Every hypothesis this refuted

| hypothesis | how it died |
|---|---|
| L2 and L1 both parked on the display pipeline | `gpu_wait` flat at 6.4–6.7 ms across a 40× swing in frame cost |
| cold glyph atlas (this document's own headline) | real stalls: `build` 0.2–0.7 ms, `glyphs` **0**, every time |
| atlas thrashing above capacity | `evict` / `rebuild` **0** even at 17,642 distinct glyphs |
| `queue.commandBuffer()` blocking | `cmdbuf` **0.0 ms**, every sample |
| dev-panel / context-menu canvases | `canvas` **0.0 ms**, every sample |

Five hypotheses, five refutations, one survivor — and the survivor was
found by a counter, not by reading the code.  The code reading did
produce the suspicion (`make_instance_buffer` is plainly a per-frame
allocation, plainly against `CLAUDE.md`'s own hot-path rule), but the
same reading produced four other suspicions that were wrong.

## The fix

`InstanceBufferPool`: one persistent `MTLBuffer` per pass, capacity
rounded up to a power of two and only ever grown, refilled by `memcpy`
into `contents()`.  Steady state allocates nothing.

Sound only where the GPU has finished with the previous frame's
contents.  The IOSurface path ends every frame in `waitUntilCompleted`,
so it qualifies; the live `CAMetalLayer` path waits only until
*scheduled* and keeps allocating per frame.  The parameter is
`Option<&mut InstanceBufferPool>` so the distinction is visible at both
call sites rather than remembered.

Synthetic: warm-frame encode **0.21 ms → 0.05 ms**; the cold frame
allocates 2.12 MB once, in 0.03 ms.

## Still open

* **The GPU-wait mode** — 85–120 ms in `waitUntilCompleted` on three
  of twelve samples.  Untouched, unexplained, smaller.
* **`encode_canvas_into`** still allocates per frame.  Measured at
  0.0 ms because the panels are usually closed; its buffers come from
  a loop over slices, so they have no fixed slot.
* **The cold-atlas cost is real but was never the stall** — 122 ms for
  8,661 glyphs, on the frame a fresh core draws before it can answer a
  PING.  It sits behind the allocation fix in the queue, not in front
  of it.

## After the fix — measured on the same machine

Eleven minutes on the live app, load 3.8–8.3:

```
388ms  build_387.3  cmdbuf_0.0  encode_0.1 (instbuf_0.0 / 0.0MB)  gpu_1.2   glyphs_0
 81ms  build_  0.3  cmdbuf_0.0  encode_0.1 (instbuf_0.0 / 0.0MB)  gpu_81.1  glyphs_0
```

**Two stalls, and `instbuf` is `0.0 ms / 0.0 MB` in both** — the
steady-state frame now allocates nothing, which is the part of this
that is certain regardless of what the machine is doing.

Before the fix the comparable window held twelve stalls, nine of them
allocation-dominated at 84–290 ms.  That drop is *not* a controlled
comparison: the machine's load fell from ~7–10 to ~3.8–8.3 over the
same period.  What can be claimed is the mechanism, not the ratio.

Two modes survive, both pre-existing and both rarer:

* **`build` 387 ms with `glyphs_0`** — a pure CPU walk of 13 panes'
  cells being descheduled under load.  New only in the sense that it
  is now the largest thing left.
* **`gpu_wait` 81 ms** — the second mode from the original twelve.


---

# The second attack: it was never the work, it was the scheduling

`gpu_exec_us` — the command buffer's own `GPUEndTime - GPUStartTime`
— was added (0.12.129) to answer one question: when a frame waits a
long time, is the GPU busy, or are we simply not being run?

## The controlled sweep

Idle bench host, one variable (background CPU hogs), 2000 frames each:

| load | frame p99 | **GPU exec p99** | **GPU wait p99** |
|---|---:|---:|---:|
| idle | 293 µs | 111 µs | 279 µs |
| 14 hogs (load 6.2) | 7,453 µs | **117 µs** | **7,425 µs** |
| 42 hogs (load 22.9) | 1,720 µs | **115 µs** | 1,546 µs |

**The GPU's own account of the frame does not move.**  111 µs idle,
117 µs under load.  The wall clock around the same wait grows 27×.
The work never got heavier; the thread waiting for it stopped being
scheduled — and while it is not scheduled, every pane is frozen and
the supervisor's PONG deadline is running.

An instrument failure worth recording: the first attempt at this
sweep used `timeout` to bound the hogs, which does not exist on
macOS.  No hog ever started, and the run produced perfectly
plausible "under load" numbers that were really idle numbers.  It was
caught only because the script printed `load1` alongside each row —
a witness unrelated to the thing being measured.  Without it the
sweep would have "shown" that load doesn't matter.

## The fix, and its A/B

macOS schedules by QoS class.  A process spawned from a shell gets an
unspecified/default class — the same tier as the batch work competing
with it.  A terminal's render loop is the definition of
`USER_INTERACTIVE`.

Same machine, same load, seconds apart, both orderings:

| | frame p99 | GPU wait p99 | worst frame |
|---|---:|---:|---:|
| default QoS | 4,953–6,082 µs | 4,912–6,042 | 9.28 ms |
| `USER_INTERACTIVE` | **323–325 µs** | 308–310 | 0.34 ms |

**p99 improves 15–18.7×, the worst frame 21×**, and under load 11–15
the result matches the *idle* baseline (293 µs).  Run order reversed
and repeated (ON 323 → OFF 4,953 → ON 324), so it is not warm-up.

Applied to the two threads a person is waiting on — L2's render loop
and L1's main thread — and to neither anything else.  Worker threads
that read transcripts or sweep state keep the default: promoting
everything promotes nothing.  L3 is deliberately excluded; a window
can hold 81 of them, and no measurement says they need it.

L1's inclusion is the one with a receipt: during the 2026-08-11
incident its own tick was measured **100.8 s late**, and that thread
runs the supervisor tick and every AppKit callback.  Default QoS was
the first link in the chain that ended in "please restart the app".

## What this leaves

* The `build` mode (42–539 ms of a CPU walk that costs 0.04 ms idle)
  was the same disease — a thread not being run — and should now be
  covered by the same cure.  **Unverified**: it needs the machine to
  be loaded again to show up at all.
* `MARSPOT_QOS=1` on the bench binary raises its own thread the same
  way.  Turning it on by default would make the gate far less
  load-flaky (three re-runs were lost to that tonight), but it changes
  what the gate measures, so it is left off and left documented.
