# marspot-agent perf budgets

Each time budget is three times the largest P95 of three runs on mini (Apple Silicon, the test profile `bin/test.sh` builds, the perf gates run alone, load average 5 to 20 from other jobs), held by `perf_gate.rs`.

| Gate | One sample | Measured P95 (3 runs) | Budget | Measured |
|---|---|---|---|---|
| `sched_decide_perf_gate` | one decision of a 64-pane scan: 16 accounts priced (shared over the scan), 8 candidates chosen among, the answer recorded | 1.476, 2.809, 2.820 µs | 8.5 µs | 2026-11-26, mini |
| `sched_alloc_perf_gate` | the candidates and terms of one resting pane's account decision over 16 accounts, then the choice | 30.625, 31.250, 30.959 µs | 94 µs | 2026-11-26, mini |
| `sched_plan_perf_gate` | one full search of the pane plan: 64 panes, 16 accounts (a quarter of them running out inside the day), 50 panes in play | 274.633, 259.942, 271.987 ms (load 9.7 to 12); again after the late cut, 108.552, 106.823, 106.452 ms (load 2 to 4.6); again once switches off an emptying account keep the reading error, 194.146, 195.941, 195.592 ms (load 3 to 6) | 824 ms | 2026-11-27, mini |
| `sched_plan_retime_perf_gate` | the standing pane plan re-timed and re-costed at the same size, 16 standing switches; the gate fails if a pass finds the structure gone or evaluates no switch | 6.907, 6.895, 6.871 ms (load 3 to 6) | 20.721 ms | 2026-11-27, mini; earlier 3.162, 3.077, 3.087 ms on 5 standing switches, and before that 159.167, 63.167, 53.791 µs timed a pass in which the standing structure no longer held and nothing was evaluated |
| `sched_plan_exact_worst_case_perf_gate` | one full search whose exhaustive pass enumerates its cap of 1024 joint choices: 4 accounts that last the day, 5 panes, each may stay or go to any of the other three (1040 simulations) | 6.095, 6.049, 6.032 ms (load 3 to 6) | 18.285 ms | 2026-11-27, mini; release 0.442, 0.441, 0.436 ms against the 5 ms design ceiling |
| `sched_plan_state_bytes` | the standing pane plan at its caps (128 switches, 64 pins) with `State` | 6,200 bytes | 65,536 bytes (a cap, not 3×) | 2026-11-27, mini |
| `sched_state_bytes` | `sched::state::State` | 1,944 bytes | 65,536 bytes (a cap, not 3×) | 2026-11-26, mini |
