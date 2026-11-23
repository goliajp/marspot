"""Gate rows for the pane control socket's latencies.

bin/measure-pane-control.sh writes `panectl.json` into the run's
directory (run by bin/bench.sh --full); each latency's P95 is held to
`pane_control.<name>_p95_ms_max` in bench/baseline.json.  A latency the
baseline has no ceiling for is shown, not gated; one the probe skipped
(shadow panes before the core opens them) has no row.
"""
import json
import os

import _bench_record

NAMES = ("send", "send_enter", "fired", "run_true", "read", "open_shadow")


def add_rows(baseline, cur_dir, mode, row):
    cfg = baseline.get("pane_control")
    if mode != "full" or not cfg:
        return
    path = os.path.join(cur_dir, "panectl.json")
    doc = json.load(open(path)) if os.path.exists(path) else None
    if doc is None:
        host = {"invalid": "no reading: bin/measure-pane-control.sh did not finish"}
    else:
        host = {"load1": {k: doc[f"load1_{k}"] for k in ("start", "max", "end")}}
        if doc["load1_max"] > _bench_record.LOAD_MAX:
            host["invalid"] = f"load1 reached {doc['load1_max']} (over {_bench_record.LOAD_MAX}) during the probe"
    for name in NAMES:
        e = (doc or {}).get(name)
        if doc is not None and e is None:
            continue
        limit = cfg.get(f"{name}_p95_ms_max")
        row(f"pane control {name} p95", e["p95_ms"] if e else None, "ms", limit, True,
            gating=limit is not None, trials=(e or {}).get("samples_ms"), host=host, stat="exact")
