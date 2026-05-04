# E — bench infrastructure fixes

> Status: queued (E1+E2+E5 must finish before A/B/C/D start)
> Master:  ../perf-attack.md

These items don't directly improve mars's perf — they fix the
*measurement system*.  Without honest, reproducible numbers,
diagnosing A/B/C/D will chase phantom regressions and miss real
ones.  Bundled here because each is small (½ day or less); split
out only if one balloons.

The set: E1 staleness guard · E2 fast-gate warm-up · E3 cross-term
vim-jump time capture · E4 stale-marker cleanup · E5 measure-other
3-trial median · E6 covered by [A2](A2-cpu-drift-gate-bug.md).

---

## E1 — competitors_snapshot staleness guard

### What's broken

`bench/baseline.json` `competitors_snapshot.captured_at` was
2026-05-03 when the 2026-05-05 `--full` gate fired.  Two-day-old
Warp numbers (47 MB/s flat across cjk/emoji — clearly stale or
test-anomalous) caused the gate to report "mars losing to iTerm2 on
cat-ascii (0.92×)" — a false alarm.  Fresh measurement actually
shows mars 1.84× iTerm2.

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

## E2 — fast-gate warm-up trial

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
"$ROOT/target/release/mars" --bench "parse:$SCENARIOS_DIR/$s.bin" >/dev/null
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

## E3 — vim-jump cross-term wall-time capture

### What's broken

`bin/scenarios/vim-jump.sh` for mars captures wall-time (mars
1.23s on 50K lines).  For iTerm2 / Terminal.app / Warp the same
scenario only captures RSS; wall-time is missing.  Cross-terminal
comparison on the vim-jump axis is impossible.

### TDD failing test

```sh
# After fix:
bin/scenarios/vim-jump.sh iterm    /tmp/vim-iterm.json
bin/scenarios/vim-jump.sh terminal /tmp/vim-term.json
# Expect each output JSON has "wall_time_s" key with a number
# Currently only "rss_delta" present in non-mars JSON outputs
```

### Implementation

Each terminal driver wraps the vim-jump command with `time -p`,
reads `/tmp/measure-<term>-vim.txt` for the timing.  Pattern
already used in `measure.sh` for cat-*; adapt to vim-jump.

### Exit criteria

- All 4 (mars, iterm, terminal, warp) produce wall-time on
  vim-jump.json
- Cross-term comparison reproduces in `bench-run.sh` snapshot
- `docs/bench.md` matrix updated to mark vim-jump as fully cross-term

---

## E4 — measure-other.sh stale-marker cleanup

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

## E5 — measure-other.sh 3-trial median

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

## E6 — active-9x-soak CPU drift gate direction

Covered fully in [A2](A2-cpu-drift-gate-bug.md).  Listed here for
completeness — same fix benefits the bench-infra hygiene story.

---

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
