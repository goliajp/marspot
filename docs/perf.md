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
| cat-ascii | 32 MB | 41.4 s | **0.77 MB/s** |
| cat-mixed | 16 MB | 25.2 s | 0.63 MB/s |
| cat-cjk   | 8 MB  | 13.2 s | 0.61 MB/s |
| cat-emoji | 2 MB  | 6.6 s  | 0.30 MB/s |

Sub-path numbers (no PTY/window in the loop):

| Sub-path | Number | How |
|---|---|---|
| Parse-only (cat-ascii)  | 93 MB/s  | `target/release/mars --bench parse:cat-ascii.bin` |
| Parse-only (cat-mixed)  | 74 MB/s  | …`cat-mixed.bin` |
| Parse-only (cat-cjk)    | 99 MB/s  | …`cat-cjk.bin` |
| Parse-only (cat-emoji)  | 102 MB/s | …`cat-emoji.bin` |
| Render-only (worst-case full repaint) | p50 891 µs · p95 957 µs · p99 1.0 ms | `--bench render:1000` |
| Typing latency (key → setContents)    | _ (self-instrumented; collect via `MARS_LATENCY=…`) |

### vs iTerm2 / Warp (cross-terminal)

> Status: **automation blocked**.  AppleScript dispatch into iTerm2 /
> Warp / Terminal.app is unreliable for benching (AppleEvent timeouts,
> profile-specific shell init, smart-paste rewriting commands).  Future
> path: vtebench-style harness, or a small self-built launcher.  For
> now, paste the commands `bin/measure.sh` prints into each terminal
> manually and fold the numbers into the table below.

| Use-case | mars | Warp | iTerm2 | mars/best |
|---|---|---|---|---|
| cat-ascii | 0.77 MB/s | _ | _ | _ |
| cat-mixed | 0.63 MB/s | _ | _ | _ |
| cat-cjk   | 0.61 MB/s | _ | _ | _ |
| cat-emoji | 0.30 MB/s | _ | _ | _ |
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
| Live throughput **100–300× slower than headless parse** (parse 93 MB/s vs live 0.77 MB/s on cat-ascii) | enormous | **yes** | Bottleneck is render scheduling, not parser. Each 4 KiB PTY chunk triggers a `request_redraw`; CALayer.setContents appears to be vsync-pinned (~60 Hz), so live drain ≈ 4 KiB × 60 ≈ 250 KB/s after coalescing.  **Fix path:** decouple render rate from chunk arrival rate — drain channel into a "dirty" flag, paint at most once per vsync; ideally also dirty-region track so a single keystroke doesn't repaint 4 758 cells. |
| Render p99 ~1 ms but live render appears ~16 ms | 16× | yes | Same root cause — vsync wait. Confirms the fix above is the right lever. |
| Per-chunk reader→channel→main_thread overhead | small | maybe | Reader chunks are 4 KiB; channel capacity 64; main thread drains all on each user_event. If contention shows up after the render fix, batch chunks in the reader (e.g. coalesce up to 256 KiB before sending). |
| Cross-terminal automation broken | n/a | yes | AppleScript / Terminal-AppleEvent path is unreliable. Either build a vtebench-style runner that drives each terminal via a custom hardware-keystroke approach, or accept manual paste for now. |

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
