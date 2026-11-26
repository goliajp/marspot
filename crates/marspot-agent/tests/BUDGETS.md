# marspot-agent perf budgets

Each time budget is three times the largest P95 of three runs on mini (Apple Silicon, the test profile `bin/test.sh` builds, the perf gates run alone, load average 5 to 20 from other jobs), held by `perf_gate.rs`.

| Gate | One sample | Measured P95 (3 runs) | Budget | Measured |
|---|---|---|---|---|
| `sched_decide_perf_gate` | one decision of a 64-pane scan: 16 accounts priced (shared over the scan), 8 candidates chosen among, the answer recorded | 1.476, 2.809, 2.820 µs | 8.5 µs | 2026-11-26, mini |
| `sched_alloc_perf_gate` | the candidates and terms of one resting pane's account decision over 16 accounts, then the choice | 30.625, 31.250, 30.959 µs | 94 µs | 2026-11-26, mini |
| `sched_state_bytes` | `sched::state::State` | 1,944 bytes | 65,536 bytes (a cap, not 3×) | 2026-11-26, mini |
