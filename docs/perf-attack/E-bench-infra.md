# E — bench infrastructure fixes

> Status: queued (E1+E2+E5 must finish before A/B/C/D start)
> Master:  ../perf-attack.md

These items don't directly improve marspot's perf — they fix the
*measurement system*.  Without honest, reproducible numbers,
diagnosing A/B/C/D will chase phantom regressions and miss real
ones.  Bundled here because each is small (½ day or less); split
out only if one balloons.

The set: E1 staleness guard · E2 fast-gate warm-up · E3 cross-term
vim-jump time capture · E4 stale-marker cleanup · E5 measure-other
3-trial median · E6 covered by [A2](A2-cpu-drift-gate-bug.md).

---

## E1 — competitors_snapshot staleness guard · **DONE 2026-05-05**

Landed in `feature/perf-E1-staleness-guard`.  bin/bench.sh now refuses
to run --full mode if competitors_snapshot.captured_at is older than 7
days; pre-flight check fires in <1s before any measurement.  BASELINE
env var honoured for fixture-based testing.  Test:
`bench/tests/e1-staleness.sh` (~1s; synthesises 30-day-old fixture).

### What's broken

`bench/baseline.json` `competitors_snapshot.captured_at` was
2026-05-03 when the 2026-05-05 `--full` gate fired.  Two-day-old
Warp numbers (47 MB/s flat across cjk/emoji — clearly stale or
test-anomalous) caused the gate to report "marspot losing to iTerm2 on
cat-ascii (0.92×)" — a false alarm.  Fresh measurement actually
shows marspot 1.84× iTerm2.

### TDD failing test

```sh
# Add to bin/bench.sh's gate evaluation:
test_e1_competitors_staleness() {
  # Set captured_at to 14 days ago in baseline.json
  # Run bin/bench.sh --full
  # Expect: gate refuses to compute vs-best ratio,
  #         emits "competitors_snapshot stale (>7 days), refresh via
  #          measure-other.sh" and exits with the existing fail code
  #         (so CI fails loudly)
}
```

Currently no such guard exists.

### Implementation

In `bin/bench.sh` gate-evaluator (the python heredoc starting
`baseline = json.load(open(baseline_path))`):

```python
from datetime import datetime, timezone
captured = baseline.get("competitors_snapshot", {}).get("captured_at")
if captured:
    age_days = (datetime.now(timezone.utc) -
                datetime.fromisoformat(captured)).days
    if age_days > 7:
        print(f"competitors_snapshot is {age_days} days old (limit 7).")
        print("Refresh via bin/measure-other.sh, then update baseline.json.")
        sys.exit(2)
```

### Exit criteria

- Gate fails with clear message when snapshot > 7 days old
- Gate proceeds when ≤ 7 days
- Test added to bench self-test suite

---

## E2 — fast-gate warm-up trial · **DONE 2026-05-05**

Landed in `feature/perf-E2-warmup`.  Each of the four measurement
loops (parse, render, scroll, scroll-cold) now runs a discarded
warm-up invocation before the timed trials.  Test:
`bench/tests/e2-warmup.sh` (structural + behavioural, load-independent
~1s runtime).

### What's broken

First `bin/bench.sh` run after a `cargo build --release` produced:

```
parse cat-ascii   136.3 ✗ FAIL  (vs floor 161)
render p99 (µs)   2145.7 ✗ FAIL (vs ceiling 1800)
scroll p99 (µs)   9.8   ✗ FAIL  (vs ceiling 4)
```

Second run minutes later (same binary, same code):

```
parse cat-ascii   167.5 ✓
render p99 (µs)   961.7 ✓
scroll p99 (µs)   2.0   ✓
```

Trial 1 is contaminated by cold filesystem cache, cold thermal,
freshly-loaded shared libraries.  The gate currently averages
trials 1-5; trial 1 noise pulls the median enough to flap.

### TDD failing test

```sh
# Reproduce the cold-build flap:
cargo clean
cargo build --release
./bin/bench.sh
# Expect: gate passes with consistent numbers (currently fails)
# After fix: trial 1 is discarded, trials 2-5 / trials 2-6 used
```

### Implementation

In `bin/bench.sh` parse-trials loop (currently `for i in 1 2 3 4 5`):
```sh
# Run a warm-up trial whose result is discarded
"$ROOT/target/release/marspot" --bench "parse:$SCENARIOS_DIR/$s.bin" >/dev/null
# Then collect the 5 measurement trials as before
for i in 1 2 3 4 5; do
  ...
done
```

Same pattern for render (3 trials) and scroll (5 trials).

### Exit criteria

- `cargo clean && cargo build --release && ./bin/bench.sh` passes
  on 3 consecutive cold builds
- Warm-up trial run-time accounted for in script's total time
  estimate (mention in `--help`)

---

## E3 — vim-jump cross-term wall-time capture · **SATISFIED 2026-06-14** (Warp gap noted)

The filed description below is **stale**: `bin/scenarios/vim-jump.sh` was
rewritten since to share one `WORKER` script across terminals that wraps
vim with `/usr/bin/time -p … 2> timing.txt`; the aggregator parses `real`
into `wall_ns`/`wall_s` for **any** terminal.  marspot / iTerm2 /
Terminal.app all produce cross-term wall-time today (`docs/bench.md`
matrix already marks vim-jump "yes (manual)").  **Remaining gap:** Warp is
not in vim-jump.sh's `case` — `bin/drivers/warp.sh` is paste-mode only
(no window-id return / no programmatic close), so a wired-in Warp run
would leak windows and can't be verified headless; left out deliberately
rather than shipped half-working.  Closing it needs a real Warp driver
(window enumeration + close), tracked there if Warp-vs ever matters.

### What's broken (stale — see above)

`bin/scenarios/vim-jump.sh` for marspot captures wall-time (marspot
1.23s on 50K lines).  For iTerm2 / Terminal.app / Warp the same
scenario only captures RSS; wall-time is missing.  Cross-terminal
comparison on the vim-jump axis is impossible.

### TDD failing test

```sh
# After fix:
bin/scenarios/vim-jump.sh iterm    /tmp/vim-iterm.json
bin/scenarios/vim-jump.sh terminal /tmp/vim-term.json
# Expect each output JSON has "wall_time_s" key with a number
# Currently only "rss_delta" present in non-marspot JSON outputs
```

### Implementation

Each terminal driver wraps the vim-jump command with `time -p`,
reads `/tmp/measure-<term>-vim.txt` for the timing.  Pattern
already used in `measure.sh` for cat-*; adapt to vim-jump.

### Exit criteria

- All 4 (marspot, iterm, terminal, warp) produce wall-time on
  vim-jump.json
- Cross-term comparison reproduces in `bench-run.sh` snapshot
- `docs/bench.md` matrix updated to mark vim-jump as fully cross-term

---

## E4 — measure-other.sh stale-marker cleanup · **DONE 2026-05-05**

Landed in `feature/perf-E4-E5-measure-other-hardening`.  Script now
removes pre-existing /tmp/measure-{terminal}-all.txt at startup
before printing paste blocks.  Stale ==ALL_DONE== markers from prior
runs no longer pollute fresh measurements.  Test:
`bench/tests/e4-e5-measure-other.sh`.

### What's broken

When `measure-other.sh` starts polling, pre-existing
`/tmp/measure-{terminal}-all.txt` files from previous runs that
already contain `==ALL_DONE==` get accepted *immediately* — without
the user pasting anything in that terminal.  Surfaced in the 2026-05-05
session: 2-day-old stale iterm + warp markers caused the script to
think those terminals had finished before they were touched.

### TDD failing test

```sh
# Reproduce:
echo "==ALL_DONE==" > /tmp/measure-iterm-all.txt
./bin/measure-other.sh
# Expect: script removes the stale marker and waits fresh
# Currently: accepts stale marker, parses (probably-zero) timings
```

### Implementation

At the top of `bin/measure-other.sh`:
```sh
for t in "${OTHER_TERMINALS[@]}"; do
  rm -f "/tmp/measure-${t}-all.txt"
done
```

### Exit criteria

- Pre-existing markers don't pollute a fresh run
- Test scripted into a self-check (drops stale, verifies wait)

---

## E5 — measure-other.sh 3-trial median · **DONE 2026-05-05**

Landed in `feature/perf-E4-E5-measure-other-hardening`.
build_command now emits TRIALS=3 invocations per (terminal, scenario);
parse_marker collects all real-time samples per scenario, sorts, and
emits the median.  TRIALS env var override available for future
soak-style 9-trial runs.  Test:
`bench/tests/e4-e5-measure-other.sh` (end-to-end with synthesised
0.20 / 0.50 / 1.00s trials → median 0.50s, distinct from "last sample"
of 1.00s so a regression to last-wins logic fails the gate).

### What's broken

`measure-other.sh` runs each scenario *once* per terminal.  Single
trial on a 32 MiB cat is high variance — measured iTerm2 cat-ascii
of 28 MB/s on 2026-05-05 vs baseline.json's 56 MB/s.  Without 3+
trials we can't tell if the regression is real or trial noise.

### TDD failing test

```sh
# After fix:
./bin/measure-other.sh
# Expect each scenario runs 3 times per terminal
# JSON output includes p50 / p95 / per-trial values
# Currently: 1 trial, single number
```

### Implementation

The `build_command` function already builds a one-liner.  Wrap
the four scenarios in `for trial in 1 2 3` loop.  Update the
parser to compute median.

### Exit criteria

- Each terminal runs 4 scenarios × 3 trials = 12 timings
- JSON has `median_ns`, `p95_ns`, `samples_ns`
- Total runtime ≤ 3× single-trial (~3-5 minutes per terminal)

---

## E6 — active-9x-soak CPU drift gate direction · **RETRACTED 2026-05-05**

Covered in [A2](A2-cpu-drift-gate-bug.md).  Investigation showed the
gate works as designed: the `cpu_q1 < 1.0` short-circuit is intentional
(sub-1% mean amplifies sample noise into meaningless drift).  godot's
"2.40× ✓" was the short-circuit firing on a low cpu_q1 from CPU
starvation, not an inverted comparison.  Closed not-a-bug.

---

## E7 — bench scripts kill marspot/mcli by name (friendly-fire) · **DONE 2026-05-05**

Discovered while running active-9x-soak --extended (30 min) in
background and a sanity bench.sh in foreground.  bench.sh's idle-RSS
loop did `pkill -x "$bin"` (marspot/mcli) before each trial, intending
to clear stale instances from prior fast-gate runs but actually
nuking the live --extended marspot binary.  At t=41 s of the 30-min
soak, marspot died; remaining 1759 s of samples were zero-RSS, drift
ratio computed as 0.0 ✓ (false-pass against the failure condition
the gate was designed to catch).

measure.sh had the same hazard (`killall mcli marspot` before spawn).

Fix landed in `feature/perf-F-recalibrate-clean`:
- bin/bench.sh idle-RSS loop: removed `pkill -x "$bin"`; the captured
  `pid=$!` is enough to manage just-our-instance.
- bin/bench.sh --full block: removed `pkill -x marspot`; measure.sh
  now manages its own mcli by PID.
- bin/measure.sh run_in_mars: replaced `killall mcli marspot` (pre)
  and `killall mcli` (post) with explicit `kill "$mcli_pid"`.
  Captured PID via direct `&` rather than nested subshell so $! is
  the mcli PID we can target.

Test: bench/tests/e7-no-friendly-fire.sh — greps bench scripts for
`pkill -x marspot/mcli`, `killall marspot`, `killall mcli` patterns
(excluding comment lines).  Currently passes.

### Why it matters

Long-running bench scenarios (active-9x-soak, especially --extended
at 30 min) overlap with the natural cadence of dev work — running a
fast-gate sanity bench while a soak is in flight should be safe.
Without E7, every concurrent invocation invalidates whatever soak
was running.  Fixing this unblocks parallel bench work and removes
a phantom-failure source from soak diagnostics.

---

## E8 — gate measures production shell→core→L3, not standalone mcli · **DONE (measurement) 2026-06-13**

### What's broken

Per-session L3 became the default architecture on 2026-06-13: every
pane's bytes flow shelld → `marspot-session` (parser → grid → shm
publish), measured at ~0.90× the in-process bulk-cat rate (the per-pump
~40 KiB shm-window memcpy + the process hop).  But the gate's "marspot
live" number came from one of two non-production sources:

- `bin/measure.sh` drives the standalone `mcli` binary — one in-process
  session, no IPC/shm hop (intentionally, so the per-session number isn't
  diluted 1/9 by `marspot`'s 9 cells).  That's ~10 % faster than the
  product ships.
- `competitors_snapshot.marspot` (152–160 MiB/s) was hand-captured via
  Screen Sharing on the **pre-L3** app — not reproducible in an automated
  gate and now architecturally stale.

So `vs-best-other` overstated marspot by the L3 hop's cost, and B3/B4/C/D
would be diagnosed against an architecture the product no longer runs.
This had to land before A/B/C/D (the whole point of bucket E).

### TDD failing test

```sh
# Before: no headless, reproducible production-path throughput exists.
bin/measure-l3.sh
# Expect: bench/results/l3-throughput.json with bytes_per_sec per cat-*
#         scenario, measured through a real marspot-session.
# Then bin/bench.sh --full announces "live source: l3-throughput.json"
# and gates that number (not the mcli/Screen-Sharing one).
```

### Implementation

- `crates/marspot-session/examples/l3_throughput.rs` — spawns a real
  `marspot-session` attached to a fresh shelld session (inherited shm
  region + control socket, exactly as `marspot-core::spawn_l3_pane`),
  types `cat <scenario> <scenario> …` over the control socket, and times
  the drain window from command-issue to the scroll_push_count plateau.
  Key correctness detail: the window is anchored at **t_issue** and ends
  at the **last count change**, not at "first advance" — the session
  parses in coarse pumps (a cached cat can land tens of MiB in one pump,
  publishing the shm only at pump boundaries), so a first-advance anchor
  read near-zero windows under burst.  Payload is the scenario file
  repeated to ~128 MiB so the window clears the 10 ms poll / 1000 ms
  plateau resolution (a 32 MiB file drains in ~0.2 s — unmeasurable).
- `bin/measure-l3.sh` — dev-sandbox wrapper (own shelld, never touches the
  installed app), loops cat-ascii/mixed/cjk/emoji × 3 trials, aggregates
  the median into `bench/results/l3-throughput.json`.  CARGO_TARGET_DIR-
  aware (re-points the sandbox shelld at the resolved target dir for the
  mini's dedicated tree).  Median-of-3 absorbs the occasional cached-burst
  outlier.
- `bin/bench.sh --full` — `load_live` now prefers `l3-throughput.json`
  (fresh ≤ 7 days) over `competitors_snapshot.marspot` over `live.json`,
  and announces which source it gated.  Non-disruptive: when L3 wasn't
  measured on a host the chain falls back to the existing snapshot.

### Exit criteria

- `bin/measure-l3.sh` produces reproducible production-path MiB/s
  (verified dev box: ascii ~95, mixed ~84, cjk ~106, emoji ~101 MiB/s,
  per-scenario trial spread ≤ 1 % after the median).  **DONE.**
- `bin/bench.sh --full` gates the L3 number and says so.  **DONE.**
- **Pending:** floors re-locked against L3 on the idle mini
  (`bin/measure-l3.sh` on the mini → `bench-remote.sh --full
  --update-baseline`).  Until then `--full` shows `live`/`vs-best` FAILs
  by design — the dev box is too noisy to lock floors on (perf-attack F
  lesson), and the floors still reflect the pre-L3 in-process numbers.

## Combined exit criteria for E

E1, E2, E3, E4, E5 all closed.  After E lands:

- The gate is reproducible (no cold-build flap)
- The vs-best ratio uses fresh data or refuses to compute
- Cross-terminal vim-jump is comparable
- measure-other.sh is robust against stale state and one-shot noise

This is the pre-condition for trusting any A/B/C/D diagnosis.

## Progress log

- 2026-05-05 — items filed from bench-run + measure-other workflow
  surfacing each issue
