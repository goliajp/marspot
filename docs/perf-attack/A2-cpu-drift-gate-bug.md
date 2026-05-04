# A2 — CPU drift gate appears mis-keyed

> Status: queued
> Master:  ../perf-attack.md
> Related: A1 (active-9x-soak family), E (bench infra)

## What's broken

`bin/scenarios/active-9x-soak.sh mars` reports:

```
CPU drift q4/q1            2.4037   ✓ (threshold 2.0×)
```

A value of **2.40** with threshold **2.0** is being marked **✓ passed**.
Either:

(a) The gate condition is inverted (lower-better treated as higher-better
    or vice versa);
(b) The threshold semantic is "fail if < 2.0×" (passing means *steeper*
    drift), which is the opposite of what we'd want;
(c) The number printed is not the same number the gate compares against.

Until verified, **CPU drift on this scenario is uncovered** — a real
regression in CPU growth over time would silently pass.

## Why it matters

Bench gates exist to catch regression silently.  A gate that lies is
worse than no gate: it manufactures false confidence.  This must
either be fixed to fail (if the value should fail) or relabelled (if
2.40 is genuinely fine), with the rationale recorded.

## TDD failing test

```sh
# Assertion to add to the bench gate test suite:
test_a2_cpu_drift_gate_direction() {
  # Synthesize a fake scenario JSON with cpu_drift = 2.5 and assert
  # the gate fails.  Synthesize cpu_drift = 1.0 and assert it passes.
  # Currently the first assertion fails (gate marks 2.5 as ✓).
}
```

Manual reproduction:
```sh
# See the bug live:
bin/scenarios/active-9x-soak.sh mars /tmp/a2-test.json
# Output shows mars 2.4037 ✓; iTerm2 0.7598 ✓; Terminal.app 1.1798 ✓
# Whatever the rule is, 2.40 must not silently ✓ if the threshold is 2.0
```

**Exit gate**:
1. Read the script logic in `bin/scenarios/active-9x-soak.sh` and
   confirm intended semantic
2. If gate is wrong: synthesised input drift=2.5 fails, drift=1.0
   passes
3. If gate is right but label misleading: rename / re-document

## Hypotheses (ranked)

1. **Inverted comparison operator** — `if drift < threshold then fail`
   instead of `>`.  Most likely; common mistake.  Read 5 lines of
   bash, confirm.
2. **Threshold is "max acceptable q1/q4" not "max q4/q1"** — i.e., the
   number printed is q4/q1 but the gate compares q1/q4.  Possible
   if the script computes both and labels them ambiguously.
3. **Gate intentionally lenient on CPU drift** because CPU naturally
   varies under sustained load.  If so, the threshold value should
   be way higher and the ✓ correct — but then label "threshold 2.0×"
   is misleading.

## Investigation roadmap

1. Open `bin/scenarios/active-9x-soak.sh`, locate `cpu_drift` /
   `q4/q1` block (~10 lines)
2. Trace: how is `cpu_drift` computed, and what's the comparison?
3. Compare to RSS drift gate in same file (which correctly fires ✗
   on 2.11×) — likely a copy-paste with one operator forgotten

## Implementation roadmap

If hypothesis 1 confirms:
- Flip comparison to `>` (fail when drift exceeds threshold)
- Re-run scenario → mars's 2.40 should now show ✗ FAIL
- Confirm iTerm2 0.76 and Terminal.app 1.18 still ✓ (under threshold)

If hypothesis 2:
- Compute drift consistently as `mean(last_quartile) /
  mean(first_quartile)`; pick one direction; document
- Same operator fix as above

Add unit test coverage:
- `bench/tests/active-soak-gate.sh` (new) with synthetic JSON inputs
  for drift = 0.5 / 1.0 / 1.5 / 2.0 / 2.5 / 5.0; assert gate
  verdict for each

## Exit criteria

1. Active-9x-soak gate fires ✗ on mars's current 2.40 CPU drift
2. Synthetic-input test suite covers all four corner combinations
   (low/high RSS drift × low/high CPU drift)
3. Threshold + comparison rationale documented in the scenario script

## Risks

- **Don't tighten the threshold while fixing the bug** unless the
  numeric value needs adjustment too.  Mixing semantic fix with
  threshold change makes regression harder to localise.
- After fix, mars will show **two ✗** on active-9x-soak (RSS + CPU
  drift).  This is correct exposure of A1's full impact, not a new
  regression.

## Progress log

- 2026-05-05 — item filed from bench-run 20260505-055755-6889ffb
- 2026-05-05 evening — investigated `bin/scenarios/active-9x-soak.sh`
  lines 219-236.  **Not a bug — gate works as designed.**

  ```python
  cpu_drift_pass = (
      cpu_drift is None
      or cpu_q1 < 1.0    ← short-circuit when q1 < 1%
      or cpu_drift <= cpu_max
  )
  ```

  Comment in source: "CPU ratio is meaningless when q1 < 1 %; pass
  through."  Reason: at sub-1% mean CPU, the ratio amplifies sample
  noise — a swing from 0.3% to 0.9% (both essentially idle) gives a
  drift of 3.0× but no actual perf event happened.

  godot session mars showed `2.40× ✓` because cpu_q1 was low (godot
  was hogging CPU during early samples → mars's cpu_q1 mean fell
  below 1%); the short-circuit engaged.  Clean session shows
  `0.69× ✓` — drift legitimately under threshold, gate behaved
  identically.

  Verdict: **A2 retracted as bug.**  The gate logic is correct.
  Re-purposing this item to a tightening proposal:

  **Optional improvement** (not blocking): supplement the q1<1%
  short-circuit with an absolute-spread check (e.g. cpu_max - cpu_min
  ≥ 5 percentage-points fails regardless of q1).  Catches the case
  where mars goes from quiet to "burning a sustained 5%" — which the
  ratio gate currently passes if q1 was 0.4%.  Whether this matters
  depends on whether such a transition is realistic; deferring until
  after A1 is resolved (A1 dominates the A-bucket).

  Status: closed (not-a-bug).  Optional tightening retained as future
  work.
