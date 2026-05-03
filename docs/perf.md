# Mars performance — budget, measurement, gaps

A living document. The first-principles requirement (CLAUDE.md): mars
must outperform iTerm2 / Warp on every realistic use-case. This file is
how we keep that honest:

1. **Budget per hot path** — what the architecture _should_ allow
2. **Measurement** — how we measure, what the numbers actually are
3. **Gaps** — current vs theoretical ceiling, with fixability triage

The benchmark gate (see `bin/bench.sh`, once built) reads the baseline
numbers committed alongside this doc and fails any merge that regresses.

---

## Use-cases we measure

These were chosen to cover the realistic perf surface — not synthetic
microbenchmarks. Each maps to "what would a user notice if mars were
slow on this?"

| ID | Scenario | What it stresses |
|---|---|---|
| `cat-ascii` | `cat 32MB ASCII text > /dev/null` (run inside terminal) | pure throughput, no escape sequences, 1-byte-per-cell |
| `cat-mixed` | `cat 16MB log file with ANSI colour codes` | parser + SGR + render pipeline |
| `cat-cjk` | `cat 8MB CJK text` | wide-char path + font fallback hot |
| `cat-emoji` | `cat 2MB emoji-heavy text` | non-BMP + colour glyph |
| `vim-jump` | `vim` opens a 50k-line file, jumps to bottom and back | escape-sequence density, scroll, cursor moves |
| `htop-60s` | run `htop` for 60s, full-screen redraws every second | sustained periodic full repaint |
| `scroll-10k` | terminal output 10k lines, scroll back to top via wheel | scrollback access + render |
| `typing-latency` | self-instrumented: keystroke → CALayer.setContents | input-to-pixel latency |

`typing-latency` is mars-only — we can't observe other terminals'
internal timing without invasive instrumentation. We track it
longitudinally to catch regressions.

---

## Theoretical lower bounds (per-byte / per-frame)

**Bytes path** (PTY → grid). Lower bound = `read()` syscall amortised + memcpy:
- 4 KiB read = 1 syscall ≈ 1 µs ≈ 0.25 ns/byte
- Branch-per-byte parser ≈ 1–2 ns/byte (modern CPU, ~3 GHz, 3–6 cycles)
- Cell write ≈ 16-byte store ≈ 1 ns
- **Floor: ~3 ns/byte ≈ 333 MB/s sustained**

**Frame path**. Lower bound for full repaint of 122×39 grid:
- 4758 cells × ~8 ns/cell (memory write + glyph index lookup) ≈ 38 µs CPU
- CGContext create ≈ 50 µs
- CTFontDrawGlyphs ≈ 1 µs/run × ~150 runs/frame ≈ 150 µs
- CGImage create + setContents ≈ 200 µs
- **Floor: ~440 µs/frame ≈ 2300 fps headroom**

These are aggressive; real measurement will land 2–5× above.

---

## Measurements

> All numbers from `bin/measure.sh`, run on user's 1x display, default
> 122×39 grid unless noted. Each cell is `p50 / p95` over 10 runs.
>
> `_` = not yet measured.

### mars baseline (release, default 122×39 grid, scale=1)

Live PTY pipeline (the harness pipes a scenario file through cat into a
fresh mars window via `MARS_SHELL`):

| Use-case | bytes | median time | live throughput |
|---|---|---|---|
| cat-ascii | 32 MB | 0.36 s | **89 MB/s** |
| cat-mixed | 16 MB | 0.24 s | 67 MB/s |
| cat-cjk   | 8 MB  | 0.16 s | 50 MB/s |
| cat-emoji | 8 MB  | 0.18 s | 44 MB/s |

> Two earlier corrections worth flagging publicly so the history is
> honest: (1) the first round of measurements claimed mars was
> 100–300× slower than headless parse — that came from an awk bug in
> `bin/measure.sh` that read POSIX `real 0.20` as `0m0.20s` (0.20
> *minutes*), inflating every live number by 60×. (2) The follow-on
> "render is the bottleneck" diagnosis was wrong: `MARS_PROFILE` showed
> only 2–3 renders happen across an entire 32 MB scenario.  Real
> bottleneck was IPC volume (one event per ~1 KiB chunk × tens of
> thousands of chunks), now mostly absorbed by reader-side opportunistic
> batching.  The remaining gap to headless parse is inside `Terminal::feed`.

Sub-path numbers (no PTY/window in the loop):

| Sub-path | Number | How |
|---|---|---|
| Parse-only (cat-ascii)  | 215 MB/s | `target/release/mars --bench parse:cat-ascii.bin` |
| Parse-only (cat-mixed)  | 215 MB/s | …`cat-mixed.bin` |
| Parse-only (cat-cjk)    | 276 MB/s | …`cat-cjk.bin` |
| Parse-only (cat-emoji)  | 254 MB/s | …`cat-emoji.bin` |
| Render-only (worst-case full repaint) | p50 900 µs · p95 987 µs · p99 1.0 ms | `--bench render:1000` |
| Typing latency (key → setContents)    | _ (self-instrumented; collect via `MARS_LATENCY=…`) |

### What `MARS_PROFILE` showed (cat-ascii, 32 MB)

| Counter | Before reader batching | After reader batching |
|---|---|---|
| user_events | 33 126 | 1 806 (-95 %) |
| bytes/chunk avg | ~1 KiB | ~19 KiB |
| render_calls | 3 | 2 |
| feed_total | 544 ms | 526 ms |
| drain_total | 573 ms | 527 ms |
| wall | 720 ms | 660 ms |

The reader-side batching (read the first chunk blocking, then `poll(0)`
+ non-blocking reads to coalesce into one 64 KiB chunk) cut event count
~18× and saved ~9 % on wall.  The remaining 660 ms is dominated by
`Terminal::feed` itself — at 64 MB/s vs the 93 MB/s headless ceiling,
the gap is parser/grid work, not IPC.  Next optimisation lever is
inside `feed` (parser branch density, scroll memcpy cost), not the
reader thread.

What this overturned: the earlier "render is the bottleneck" finding
was wrong on two counts.  (1) The 60× number came from an awk bug
in `bin/measure.sh`'s timing parser (POSIX `real 0.20` was being read
as `0m0.20s` → 0.20 minutes → 12 s).  (2) `MARS_PROFILE` shows we
render 2–3 times for a whole 32 MB scenario — every PTY chunk's
user_event handler drains the entire channel before yielding, so most
"events" find an empty channel and don't redraw.  Render isn't the
slow path; the slow path is parser-internal.

### vs iTerm2 / Warp (cross-terminal)

> Status: **automation blocked**.  AppleScript dispatch into iTerm2 /
> Warp / Terminal.app is unreliable for benching (AppleEvent timeouts,
> profile-specific shell init, smart-paste rewriting commands).  Future
> path: vtebench-style harness, or a small self-built launcher.  For
> now, paste the commands `bin/measure.sh` prints into each terminal
> manually and fold the numbers into the table below.

| Use-case | mars | Warp | iTerm2 | mars/best |
|---|---|---|---|---|
| cat-ascii | 89 MB/s | _ | _ | _ |
| cat-mixed | 67 MB/s | _ | _ | _ |
| cat-cjk   | 50 MB/s | _ | _ | _ |
| cat-emoji | 44 MB/s | _ | _ | _ |
| vim-jump  | _ | _ | _ | _ |
| htop-60s (avg CPU) | _ | _ | _ | _ |
| scroll-10k | _ | _ | _ | _ |

---

## Gaps & fixability triage

Filled in as `bin/measure.sh` results land. Each row gets:

- **Gap**: how far we are from theoretical floor
- **Fixable**: y / n / maybe — quick judgement, with one-line reason
- **Note**: what to do (or what to remember to do)

| Item | Gap | Fixable | Note |
|---|---|---|---|
| Live ~40% of headless parse on cat-ascii (89 / 215 MB/s) | 2.4× | partly | Remaining gap is PTY syscall + reader thread + per-event NSRunLoop dispatch (~150 ms on 32 MB scenarios after batching).  Can be tightened further with read-side coalescing or a CADisplayLink-driven main loop, but cost-vs-benefit isn't obvious until we have a cross-terminal baseline. |
| Live cat-emoji 44 MB/s vs cat-ascii 89 MB/s | 2× | yes | Color-glyph path is intrinsically expensive (CTFontDrawGlyphs through SBIX/COLR), and emoji-dense lines push more glyphs through it.  Mitigations: pre-warm font cache for common emoji blocks; aggregate same-font same-fg runs across rows. |
| Cross-terminal automation broken | n/a | yes | AppleScript dispatch into iTerm2 / Warp / Terminal.app is unreliable.  Either build a vtebench-style runner that drives each terminal via a custom hardware-keystroke approach, or accept manual paste for now. |

---

## Gate construction (post-measurement)

Once Phase 1 runs:

1. For each scenario where mars ≥ Warp/iTerm2 → set baseline at observed
   p50, gate at p95 + observed natural variance × 1.5.
2. For each scenario where mars < Warp/iTerm2 → not in gate yet. Logged
   in **Gaps** above. Fix or accept (with reason). Re-add to gate after
   the fix lands.
3. The **vs-best ratio** is also a gated metric — relative advantage
   must not shrink. This is the literal "outperform iTerm2 / Warp" check.
