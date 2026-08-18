# Marspot performance — budget, measurement, gaps

A living document. The first-principles requirement (CLAUDE.md): marspot
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
microbenchmarks. Each maps to "what would a user notice if marspot were
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

`typing-latency` is marspot-only — we can't observe other terminals'
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

### marspot baseline (release, default 122×39 grid, scale=1)

Live PTY pipeline (the harness pipes a scenario file through cat into a
fresh marspot window via `MARSPOT_SHELL`):

| Use-case | bytes | median time | live throughput |
|---|---|---|---|
| cat-ascii | 32 MB | 0.36 s | **89 MB/s** |
| cat-mixed | 16 MB | 0.24 s | 67 MB/s |
| cat-cjk   | 8 MB  | 0.16 s | 50 MB/s |
| cat-emoji | 8 MB  | 0.18 s | 44 MB/s |

> Two earlier corrections worth flagging publicly so the history is
> honest: (1) the first round of measurements claimed marspot was
> 100–300× slower than headless parse — that came from an awk bug in
> `bin/measure.sh` that read POSIX `real 0.20` as `0m0.20s` (0.20
> *minutes*), inflating every live number by 60×. (2) The follow-on
> "render is the bottleneck" diagnosis was wrong: `MARSPOT_PROFILE` showed
> only 2–3 renders happen across an entire 32 MB scenario.  Real
> bottleneck was IPC volume (one event per ~1 KiB chunk × tens of
> thousands of chunks), now mostly absorbed by reader-side opportunistic
> batching.  The remaining gap to headless parse is inside `Terminal::feed`.

Sub-path numbers (no PTY/window in the loop):

| Sub-path | Number | How |
|---|---|---|
| Parse-only (cat-ascii)  | 215 MB/s | `target/release/marspot --bench parse:cat-ascii.bin` |
| Parse-only (cat-mixed)  | 215 MB/s | …`cat-mixed.bin` |
| Parse-only (cat-cjk)    | 276 MB/s | …`cat-cjk.bin` |
| Parse-only (cat-emoji)  | 254 MB/s | …`cat-emoji.bin` |
| Render-only AppKit (worst-case full repaint)  | p50 944 µs · p95 1081 µs · p99 1158 µs | `--bench render:1000` |
| Render-only Metal  (worst-case full repaint)  | p50 280 µs · p95 383 µs · p99 800 µs   | `--bench metal-render:1000` |
| Typing latency Metal+local-echo (key → present)| p50 213 µs · p95 263 µs · p99 288 µs   | `MARSPOT_METAL=1 bin/scenarios/typing-latency.sh marspot …` |

### What `MARSPOT_PROFILE` showed (cat-ascii, 32 MB)

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
as `0m0.20s` → 0.20 minutes → 12 s).  (2) `MARSPOT_PROFILE` shows we
render 2–3 times for a whole 32 MB scenario — every PTY chunk's
user_event handler drains the entire channel before yielding, so most
"events" find an empty channel and don't redraw.  Render isn't the
slow path; the slow path is parser-internal.

### vs iTerm2 / Warp (cross-terminal)

> Automation status: AppleScript-driven dispatch into iTerm2 (`tell
> application "iTerm" … write text`) and Warp (`open -a Warp`,
> System Events keystroke) both work — the earlier "automation
> blocked" note was a self-inflicted misdiagnosis (the verification
> path was reading marker files via `cat`, hitting the user's
> `cat=bat` zsh alias, and reporting bat's "file not found" as if
> the marker was missing).  `bin/measure-other.sh` keeps a manual
> paste fallback.  All numbers below are median of 1 run × 4
> scenarios (single-shot per terminal — re-running for tighter
> intervals is on the TODO).

| Use-case | marspot | iTerm2 | Warp | marspot / best other |
|---|---|---|---|---|
| cat-ascii (32 MB) | **89 MB/s** | 56 MB/s | 47 MB/s | **1.6×** faster |
| cat-mixed (16 MB) | **67 MB/s** | 29 MB/s | 37 MB/s | **1.8×** faster |
| cat-cjk   (8 MB)  | **50 MB/s** | 5.8 MB/s | 47 MB/s | **1.06×** faster |
| cat-emoji (8 MB)  | 44 MB/s | 2.2 MB/s | **47 MB/s** | 0.94× — Warp slightly ahead |
| vim-jump | _ — cross-terminal driver bug, marspot-only 1.23 s, see below | | | follow-up |
| htop-60s (CPU mean) | see Multi-session section — marspot 0.0% vs iTerm2 17.8% vs Term.app 0.0% | | | win |
| scroll-10k | _ — deferred (wheel events not scriptable cross-terminal, see docs/bench.md) | | | n/a |

**Headline:** marspot wins 3 of 4 scenarios outright.  vs iTerm2 the gap
on text-heavy CJK / emoji is **an order of magnitude or more** (iTerm2
clearly hasn't optimised those paths).  vs Warp the contest is
tighter — they're both fast — and Warp pulls ahead on cat-emoji,
suggesting a frame-coalescing strategy that drops intermediate frames
when the input rate is high.  An obvious next investigation.

---

## Latency the gate cannot see (2026-07-21)

Every scenario in `bin/bench.sh` measures **throughput** — bytes/s
parsed, p99 µs per scroll or frame — against a steady-state workload.
That is the right shape for the bytes path, and it is why the parse and
scroll floors have held.  It is also blind to the class of bug that
dominated this month:

> a single iteration of a main loop blocking for seconds, once.

A 2.87 s freeze does not move a p99 taken over thousands of scroll
operations, and it does not move MB/s at all.  Five such bugs shipped
and were fixed (see `architecture.md`, "Main-loop blocking discipline");
**none of them would have failed the gate**, before or after.  They were
found by users reporting "the pane went dead for a while".

### What covers it today

`LoopWatch` (`marspot-term/loop_watch.rs`) times each iteration of both
loops, splits it into named phases, and logs one `l2.loop.stall` /
`l3.loop.stall` line past a threshold (L2 80 ms, L3 150 ms).  That is
**detection, not a gate**: it writes to the log of a running app.  It
turns "a pane froze and I don't know why" into "phase X took 2.87 s",
which is what closed the last two, but nothing fails a build because of
it.

### What a real gate would need

Not built — recorded so the shape is agreed before someone improvises
one:

1. **Adversarial workloads, not steady ones.**  The stalls fired under
   conditions the bench never creates: a foreground process that stops
   reading stdin, a disk busy with someone else's build, a peer that
   stops draining a socket.  A latency gate has to *induce* those, the
   way `write_does_not_block_on_a_child_that_ignores_stdin` does at unit
   scale.
2. **Max, not percentile.**  The metric is "worst single iteration",
   because one 2 s freeze is a worse experience than a uniformly 20 %
   slower terminal.  p99 over a long run hides exactly the event we care
   about.
3. **A `LoopWatch` assertion, not a wall-clock timer.**  The detector
   already exists and already attributes time to a phase; a gate should
   assert `stall_count() == 0` over a scripted scenario rather than
   re-derive timing from outside.

### Attempted 2026-07-21, did not work

An integration test was written along these lines — spawn a real L3
whose child raws the tty and never reads stdin, flood it with pasted
input, then assert the sandbox log holds no `l3.loop.stall` line.  It
passed.  It also passed with the pre-fix blocking `write(2)` restored
and the session binary genuinely rebuilt around it (verified: the temp
edit was present in the synced tree and the workspace recompiled), so it
proved nothing and was removed rather than left green.

Two things that cost time and are worth knowing before the next attempt:

* **`LoopWatch` reports at the *end* of an iteration.**  While a loop is
  wedged it logs nothing at all, so a test that samples the log during
  the stall sees a clean file no matter how stuck the loop is.  The wait
  has to outlast the blocking condition, not just the injection.
* **Rebuild the binary under test, not just the test's own crate.**
  `cargo nextest run -p marspot --bin marspot-core` leaves
  `marspot-session` stale, so a "does it catch the bug?" control can
  silently exercise the already-fixed binary.

Why it still didn't reproduce is unresolved: with the blocking write in
place and the child provably in raw mode, the flood produced no stall
line.  Either the pastes are not reaching the PTY write on that path, or
the write is not blocking under these conditions.  That question is the
starting point for the next attempt.

Until a gate exists, the honest statement is: **marspot has no automated
protection against a regression that reintroduces main-loop blocking.**
The unit tests pin the specific mechanisms that were fixed (blocking PTY
write, unbounded queues, handshake deadline), which is narrower than a
gate but is what stops those exact bugs from coming back.

---

## emoji parse — 2.17× (2026-08-18)

The scenario that had quietly rotted.  Worth recording in full: the
measurement mistakes cost more time than the fix did.

**What raised it.**  A live `cat` of `cat-emoji.bin` on mini took
175 ms; the 6-08 binary, rebuilt and run on the same host on the same
day against the same file, took 110 ms.

**Two装置 checks before believing that.**  (1) A GUI session
(`sudo launchctl asuser 501 …`, so the process lands in the Aqua
bootstrap namespace) produced sample-for-sample identical numbers to a
plain ssh run — the ssh environment was not the cause.  (2) `git
bisect` over the 542-commit window "found" a 13-line keyboard-mapping
commit, which cannot affect `cat` throughput.  It was right about the
numbers and wrong about the cause: another project was compiling on
mini, and the same commit measured 130 ms and 290 ms depending on when
it ran.  **Interleaved A/B taking the minimum** (load can only add
time) gave a clean separation: old 110 ms, HEAD 180 ms, five rounds,
zero overlap.

**Classification before attack.**  Headless parse, same host, same day:

| scenario | 6-08 | HEAD |
|---|---|---|
| cat-ascii | 183 MB/s | 429 MB/s |
| cat-mixed | 182 | 257 |
| cat-cjk | 222 | 232 |
| **cat-emoji** | **208 (40.3 ms)** | **75 (111.2 ms)** |

Parse alone lost 71 ms; live lost 70 ms.  The regression was entirely
in the parser's emoji path — everything downstream was innocent, and
three of the four scenarios had got substantially *faster* over the
same period.

**What the profile said, and what the corpus said.**  `sample` put
38 % of the time in Unicode table lookups.  The corpus explained why:
`🚀 ✨ 🎉` is single-codepoint emoji separated by *single spaces* —
**zero ZWJ, zero VS16** — so the ASCII lane (needs ≥ 2 chars) and the
quad lane (needs ≥ 8 bytes) both came up short on every glyph, and
each one paid the full UAX #29 cluster machine: 84 ns per non-ASCII
character, against 12.9 ns for CJK, which reaches the wide lane.

**Five changes, priced separately** (local interleaved A/B, min-of-5,
ms for one pass over the 8 MB corpus):

| | change | ms | gain |
|---|---|---|---|
| — | baseline | 113 | — |
| 1 | `char_width` no longer computes `is_ambiguous_width` unconditionally | 105 | +7 % |
| 2 | single-codepoint emoji admitted to the fast class | 81.6 | +22 % |
| 3 | quad lane threshold 8 → 4 bytes | 70.5 | +13 % |
| 4 | fast class `gbp()` lookup → two range exclusions | 54.5 | +23 % |
| 5 | lanes dispatched on the first byte | 52.5 | +4 % |

**2.17× cumulative, no corpus slower**: ascii +1.2 %, mixed +6.5 %,
cjk +3.7 %.  A shared hot path has to be re-measured across the whole
corpus matrix — measuring only the corpus the change was aimed at
cannot see an icache or layout tax landing on the others.

**Why #2 and #4 are safe.**  The fast class's existing invariant is
that a batch commit always leaves the LAST character buffered, so a
VS16 / ZWJ / skin-tone modifier arriving next still meets an open
cluster on the slow path.  What must be excluded is anything whose
boundary depends on a neighbour: regional indicators (GB12/13 pairs
🇯🇵 into ONE cluster) and skin-tone modifiers (GBP=Extend).  #4
replaces a table lookup with ranges, which can go stale, so
`the_fast_pictograph_class_never_outruns_the_tables` walks the entire
codepoint space and re-derives the class from the tables themselves —
a Unicode update adding a third exception fails the build instead of
silently splitting a cluster.

**Correction, same day.**  The paragraph that used to stand here said
the live pipeline was still 1.38× behind June and named `scroll_up` as
the next target.  Both halves came from measurements taken without
interleaving at N=3, and a proper interleaved N=5 run contradicts them:

| corpus | t5 vs the 6-08 build, live, N=5 |
|---|---|
| cat-emoji | ±0.0 % |
| cat-cjk | +20.8 % |

The live pipeline is **not** behind; `MARSPOT_DISK_SCROLLBACK=0` moves
it by ±4 %, i.e. inside the noise.  What had genuinely regressed was
the parser, by 2.1×, and that is what the five changes above fixed.
The same three-arm ladder (`bin/ab-live.py old=… head=… t5=…`) shows
emoji live at 69.5 → 147.5 MB/s across 134 MB, matching the 2.17×
measured headless.

Two runs of the *same binaries* an hour apart said −25 % and ±0 %.
That gap is the whole lesson: on a shared host, a live A/B needs
interleaved arms, the minimum rather than the median, and N raised
until two runs agree — never a pick between them.  The mechanics are
in `docs/bench.md` §2b; the tool that enforces them is
`bin/ab-live.py`.

**Actually still open**: `scroll_up` + `memmove` is ~20 % of emoji
parse in the headless profile (one 122-cell copy into scrollback plus
a 122-cell blank per line, while the corpus fills only 47 columns).
That profile runs the Memory scrollback variant; production runs the
File variant, so this needs a live-path profile before it earns an
attack — the §11 pre-Phase-B gate, not a guess.

## Where marspot actually stands (2026-08-18, certified)

The first cross-terminal numbers in this repo that survive their own
methodology.  Same host, 11 minutes apart, 32 MiB per trial, each trial
ending with a DSR round-trip so the terminal certifies it consumed the
corpus (see `docs/bench.md` §2b for why every earlier live number is
suspect).

| corpus | marspot live | ghostty live | | marspot headless parse |
|---|---|---|---|---|
| cat-ascii | 104.9 MB/s | 101.7 | +3.0 % | 393–411 |
| cat-mixed | 101.7 | 101.7 | ±0 | 240–260 |
| **cat-cjk** | **101.7** | **145.9** | **−43.5 %** | 224–236 |
| **cat-emoji** | **93.2** | **115.7** | **−24.1 %** | 139–153 |

Two things to read off this, neither of which the old harness could
have said:

**We lose CJK and emoji.**  That is the gap to close, and it is not the
parser: headless parse does 224–236 MB/s on CJK while the live pipeline
delivers 101.7.

**ascii and mixed both sit at ~101 MB/s for BOTH terminals**, which
looks like a tie and probably isn't — two independent implementations
landing on the same number usually means something else is the
constraint.  It is not the pipe, because ghostty reaches 145.9 on CJK
through the same PTY.  Worth identifying before reading either column
as a win.

### Phase A, first pass (2026-08-19)

Done with `--bench pty-raw|pty-drain|pty-feed` (the `bench-pty`
feature) plus a `:<chunk>` argument on `--bench parse`.  Per corpus,
converted to ms/MB so the segments add up:

| segment | cjk | what it adds |
|---|---:|---|
| `pty-raw` | 5.7 | child `write`s + kernel tty + `read` syscalls |
| `pty-drain` | 5.7 | reader thread, channel, per-batch `Vec` |
| `pty-feed` | 8.1 | `Terminal::feed` |
| live | 9.8 | event loop + Metal |
| **ghostty, whole pipeline** | **6.85** | |

Three findings, two of which killed a hypothesis:

**Chunking is not the problem.**  The live path feeds the parser
64 KiB at a time and the batch lanes scan contiguous slices, so a run
spanning a chunk boundary is lost — a plausible story for why CJK
suffers most.  Measured: 64 KiB chunks are within 1 % of one-shot feed
on all four corpora.  Dead.

**Our gather architecture is nearly free.**  `pty-raw` (a bare
blocking `read` loop, no thread, no channel, no `Vec`) matches
`pty-drain` (the real reader thread) to within noise.  So the 5.7
ms/MB is the kernel and `cat`, which ghostty pays too.

**Which makes ghostty's 6.85 ms/MB for the WHOLE pipeline the finding.**
It is less than our gather stage alone.  Since neither implementation
can undercut the kernel, the only way it fits is if ghostty's parse
runs in the shadow of its gather — its own comments describe a gather
stage running up to 4 buffers ahead of a parse stage.  Ours are on
separate threads too (reader thread gathers, main thread feeds), and
the channel is 64 batches deep, so the pipelining *should* already
happen — but `pty-feed` measures as gather **plus** parse (8.1 = 5.7
+ 2.4), not the max.  **Why the overlap does not materialise is the
next question**, and it is worth ~2.4 ms/MB on CJK, which is most of
the gap to ghostty.

Also measured and shipped: the reader thread was spending 85 % of its
samples in `poll`, because macOS hands a pty master ~1 KiB per read
(1009.2 B measured over 209 066 reads) and the loop slept on the first
empty probe.  Bridging up to 16 empty probes cut gather batches 6.4×
(4362 → 677) — and bought only +4-6 % throughput, which is itself the
useful part of the result: on this path the sleeps were not the cost.
Shipped anyway (real, zero-risk), but it is not the answer to the
24-43 % gap.

### Next round: why parse does not hide behind gather

The parser is now 2.17× faster on emoji and within noise of the June
build everywhere else, and headless parse runs 2.3–3.9× faster than
the live pipeline delivers.  **That ratio is the target.**  Per
`.claude/rules/perf-attack.md` the shape of the work is fixed:

1. **Phase A, read-only.**  Split one `cat` of a corpus into 18+ stages
   from `read()` on the PTY to the frame on screen, side by side with
   ghostty's equivalent path — it is open source (Zig), so this is
   reading, not guessing.  Budget must reconcile to ±20 % of measured
   wall time or the decomposition is not finished.
2. **Pre-Phase-B gate.**  `perf record` the live path (not the headless
   bench — they run different scrollback variants) and confirm the
   top attack owns ≥ 10 pp of self-time.  The §11 gate exists because
   an earlier project spent five commits removing a memcpy that was a
   tax, not a bottleneck.
3. **Then attack**, priced per change, with the whole corpus matrix
   re-measured after each.

Known input to Phase A, already measured: `scroll_up` + `memmove` is
~20 % of emoji parse in the headless profile (a 122-cell copy into
scrollback plus a 122-cell blank per line, on a corpus that fills 47
columns).  It is a candidate, not a conclusion — the live path runs the
File scrollback variant with a different cost shape.

Still unmeasured: iTerm2 / Warp / Terminal.app under the certified
protocol.  They cannot be driven from ssh (AppleEvent −1712 under both
plain ssh and `launchctl asuser` for iTerm), so their rows in
`bench/baseline.json` remain 2026-06-07 numbers taken under the old,
uncertified, mixed-byte-count protocol.  One console run of
`bin/measure-other.sh` on the bench host refreshes all three.

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
| **marspot 9-session idle RSS = 229 MiB; mcli 1-session idle RSS = 127 MiB** | huge | **yes** | Per-session scrollback ring is pre-allocated up-front: `10 000 lines × 122 cols × 16 B/cell ≈ 19 MiB` per session.  9 × 19 ≈ 170 MiB just sitting unused at idle.  Two fix paths: (1) shrink `DEFAULT_SCROLLBACK_LINES` from 10 000 → 1 000 (cuts to ~1.9 MiB/session); (2) **lazy-allocate** the ring — `vec![Cell::default(); cap × cols]` becomes `Vec::with_capacity(cap × cols)` with rows appended only on `push_line`.  Lazy alloc preserves the unbounded-history affordance while keeping idle footprint flat. |

---

---

## Multi-session / unlimited-scroll scenarios (the no.1 value)

These scenarios go beyond "be fast on a byte stream" and validate
marspot as a multi-session terminal with bounded growth — the property
that distinguishes the product. Captured by `bin/bench-run.sh`,
schema in `docs/bench.md`, drivers track per-window IDs so the user's
existing iTerm2 / Terminal.app windows are never disturbed.

Numbers below are from run id `20260504-141951-ea25e62` — the first
single-trial sweep after the **scrollback anon-mmap fix** landed
2026-05-04. The fix replaced the file-backed mmap (file→anon COW on
every first page write) with anonymous mmap; anon pages still page
out under memory pressure (now to swap rather than the file), so
the bounded-forever architecture is preserved.  Surfaces as a
+48 % multi-session-9x throughput improvement and recovers the
~10 % parse-path regression that built up across the disk-scrollback
phase work — see "Multi-session-9x" below for before/after.

Single-trial numbers vary 10-20 % with machine thermal / open-app
state; for headline / lock-the-floor work always run
`bin/bench-run.sh` ≥3 times and median outside the harness.  Re-run
to refresh; the bench gate (`bin/bench.sh`) consumes the latest
snapshot symlinked at `bench/results/cross-terminal.json`.

### Multi-session-9x (cat-mixed × 9 parallel sessions)

9 workers each `cat 16 MiB ANSI-coloured log` simultaneously into 9
marspot sessions / 9 iTerm2 windows / 9 Terminal.app windows. Aggregate
throughput is total bytes / wall-time — what the user sees as "how
fast does the terminal drain when 9 sessions are all spewing."

| Terminal     | Wall    | Aggregate throughput | RSS Δ peak | marspot / this |
|--------------|---------|---------------------:|-----------:|------------:|
| **marspot**     | 1.00 s  | **143.6 MiB/s**      | 14 MiB     | —           |
| Terminal.app | 2.41 s  | 59.9 MiB/s           | 228 MiB    | **2.40×**   |
| iTerm2       | 7.84 s  | 18.4 MiB/s           | 1710 MiB   | **7.80×**   |

marspot is **2.40× faster than Terminal.app** and **7.8× faster than
iTerm2**.  RSS at peak is 16× lighter than Terminal.app and 122×
lighter than iTerm2 — anon mmap with lazy commit means we only
fault pages that are actually written during the run; in this
single-trial 1 s scenario most ring slots stay un-touched.

**Disk-path regression history (resolved 2026-05-04).** Earlier
disk-scrollback Phase work used a file-backed mmap (MAP_SHARED →
MAP_PRIVATE on a scratch file); the file→anon COW on first write
to each page surfaced as ~6 % multi-session-9x and 5-15 % single-
session parse regressions vs the pre-disk-scrollback baseline.
Switching the mmap backing to `MAP_ANON | MAP_PRIVATE` (-1 fd, no
file involved) eliminated both: the kernel's swap path handles
eviction-on-pressure with the same outcome as the file path did,
without the COW step.  Pre-fix disk-on multi-session-9x median (3
trials): 96.7 MiB/s; post-fix (this snapshot): 143.6 MiB/s — a
**+48 % improvement**, vs-Terminal ratio +50 %.  See `src/scrollback.rs`
DiskScrollback::new comment block.

### Scrollback-1m (1 M lines, ~96 MiB) — single session

Push 1 000 000 numbered lines into one session. marspot uses mcli
(single-session) since `marspot` auto-spawns 9.

| Terminal       | Push throughput | RSS Δ post |
|----------------|----------------:|-----------:|
| **marspot (mcli)**| **97.7 MiB/s**  | n/a (mcli exited) |
| Terminal.app   | 54.5 MiB/s      | 12 MiB     |
| iTerm2         | 60.2 MiB/s      | 31 MiB     |

marspot is **1.62× faster than iTerm2**, **1.79× faster than Terminal.app**.
Disk-backed scrollback is default-on (`MARSPOT_DISK_SCROLLBACK=0` opts
out for regression bisects); a single mmap'd ring file at
`~/Library/Caches/marspot/scrollback` holds ~26 K lines per session.
Per the bounded-forever commitment, soak test
`soak_disk_scrollback_bounded_under_million_lines` asserts RSS growth
< 5 MiB and disk file growth = 0 after 1 M lines.  Single-session
disk path is throughput parity with the legacy memory path (writes
are direct memcpy through the mmap region, reads are slice access,
kernel page cache is the LRU).  Multi-session is the regime where
disk path costs measurable overhead — see the multi-session-9x note
above.

### Idle-9x (60 s, 9 idle sessions)

CLAUDE.md commits to "idle CPU ~0% / no RSS creep over hours."
This bench leaves 9 sessions idle, samples every 5 s for the
configured duration, fails if mean CPU > 5 % or last-quarter-mean
RSS Δ > 1.10 × first-quarter-mean RSS Δ.

| Terminal     | CPU mean | CPU max | RSS drift q4/q1 | Gate     |
|--------------|---------:|--------:|----------------:|----------|
| **marspot**     | **0.11 %**| 3.7 %  | 1.020×          | ✓        |
| Terminal.app | 0.01 %   | 0.4 %   | 1.000×          | ✓        |
| iTerm2       | **21.44 %**| **37.6 %** | 0.83× (noisy) | **✗ FAIL** |

iTerm2's CPU figure is partly attributable to the user's pre-existing
windows (single iTerm2 process — we can't separate "9 new idle windows"
from "the rest of iTerm2" since they share the same RSS / CPU
accounting), but the **gap is architectural**: adding 9 marspot sessions
costs ~0 % CPU and ~100 MiB RSS regardless of pre-existing state;
adding 9 iTerm2 windows palpably does not.

For longer / harder soak: `bin/scenarios/idle-9x.sh marspot <out> --extended`
runs 30 minutes — the right invocation before declaring a release.

### htop-60s (sustained periodic full-screen redraw)

Run `htop` in one window for 60 s, sample CPU + RSS at 1 Hz.  This
catches anything that turns periodic-full-redraw into a CPU sink.

| Terminal     | CPU mean | CPU max | RSS Δ max |
|--------------|---------:|--------:|----------:|
| **marspot**     | **0.0 %**| 0.1 %   | 86 MiB    |
| Terminal.app | 0.0 %    | 0.0 %   | 9 MiB     |
| iTerm2       | **17.8 %**| 36.6 % | 5 MiB     |

marspot and Terminal.app are essentially zero — htop's once-per-second
full repaint never escapes the dirty-skip / coalescing path.
iTerm2 burns 17.8 % CPU mean / 36.6 % peak just rendering an idle
htop screen, the same architectural gap visible in idle-9x.

### vim-jump (escape-density + scroll stress)

`vim -u NONE -S jump.vim 50000-line.txt` — script does
`G / redraw / sleep 300ms / gg / redraw / sleep 300ms` × 2 then
quit.  Stresses cursor-move + scroll redraw + syntax-highlight
escape-density.  Drives vim entirely via `-c` script — no
keystrokes injected.

| Terminal     | Wall    | RSS Δ post |
|--------------|--------:|-----------:|
| **marspot**     | 1.23 s  | 95 MiB     |
| iTerm2       | _       | 1 MiB      |
| Terminal.app | _       | 8 MiB      |

**Cross-terminal numbers pending:** the iterm / terminal drivers
inject the worker via AppleScript `write text`, but vim's
`redraw / sleep / quit` script doesn't reliably write its `time -p`
timing file when run that way (RSS deltas suggest vim ran briefly
or not at all).  Tracked as a follow-up driver fix; for now the
vim-jump scenario is marspot-only.

### scroll-10k (deferred)

`docs/bench.md` notes scroll-10k as "partial — wheel events not
scriptable cross-term."  Until we either build a hardware-keystroke
runner (CGEvent) or accept manual paste, this scenario stays out of
the gate.  Logged so it isn't forgotten.

### Typing-latency (marspot-only)

Drives 100 keystrokes via osascript System Events at 30 ms cadence,
each keystroke timestamped at key-down and matched with the next
`layer.setContents` (or Metal `presentDrawable`) to record (t1 - t0) ns.
Cross-terminal not possible without external screen capture — this
is marspot-vs-marspot regression.

#### Numbers (single trial, M4 Pro, 100 keystrokes)

| config                                    |   p50 |   p95 |   p99 | render avg |
|-------------------------------------------|------:|------:|------:|-----------:|
| **Metal renderer + local-echo (default)** |  **213** |  **263** |  **288** |    **162** |
| AppKit renderer + local-echo              |  1486 |  1688 |  1785 |       1379 |
| AppKit, no local-echo (pre-2026-05-04)    |  1044 |  1177 |  1188 |          — |

(All numbers in µs.  "render avg" is the per-call render time
captured via `MARSPOT_PROFILE`; "p*" is end-to-end key-down → first
frame containing the keystroke.)

#### What changed across this push

Three independent improvements stacked:

1. **render-floor** (commit `e743263`) — bitmap-context reuse +
   per-row decode + scratch-buffer lifting.  AppKit render p50
   1101 → 944 µs (-14 %) on the headless `--bench render` path.
2. **metal-renderer** + **metal-perf** (commits `41e8ac1` →
   `4346309`) — CAMetalLayer + glyph atlas + instanced quads.
   Headless render p50 1017 → 280 µs (-72 %, 3.6×) on the same
   `--bench render` workload, available as `--bench metal-render`.
3. **local-echo** (commit `0d6c26a`) — predict printable-ASCII
   keystrokes ahead of the PTY echo, validate + roll back on
   mismatch.  Takes the PTY round-trip off the typing-latency
   critical path.  In real shell use against bash/zsh, prediction
   hit rate is effectively 100 % — mismatches happen only when
   shell prints something between keystrokes.

The combined effect is **4.9× faster typing latency** than the
pre-push baseline (1044 µs → 213 µs) and lands inside the
150-300 µs band predicted by the theoretical-floor analysis above.

Note the AppKit-vs-Metal gap on the same `local-echo` code today
is **7.0×** (1486 µs vs 213 µs).  With local-echo, render time is
the dominant cost — so Metal's render advantage shows up almost
1:1 in typing-latency.

---

## Reading the bench

- **Snapshot**: `bench/results/<runid>.json` is the truth — full matrix
  in one file, machine + git fingerprint embedded.
- **Time-series**: `bench/results/timeseries.jsonl` — one line per
  (run, scenario, terminal); use it to chart "did this metric move
  over the last 30 commits" without re-parsing every snapshot.
- **`docs/perf.md`**: the narrative.  Numbers in the tables above are
  the most recent baseline.  Don't hand-edit them — re-run
  `bin/bench-run.sh --quick` and copy the printed numbers in.

## Gate construction (post-measurement)

Once Phase 1 runs:

1. For each scenario where marspot ≥ Warp/iTerm2 → set baseline at observed
   p50, gate at p95 + observed natural variance × 1.5.
2. For each scenario where marspot < Warp/iTerm2 → not in gate yet. Logged
   in **Gaps** above. Fix or accept (with reason). Re-add to gate after
   the fix lands.
3. The **vs-best ratio** is also a gated metric — relative advantage
   must not shrink. This is the literal "outperform iTerm2 / Warp" check.
