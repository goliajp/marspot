#!/usr/bin/env python3
"""Which machine bench/baseline.json belongs to, and rewriting its floors.

  _bench_baseline.py check <baseline.json> <update 0|1>
  _bench_baseline.py --self-test

A wall-clock floor or a tail latency read on one machine says nothing
about another, so the baseline names the machine its floors were locked
on (`host.name`, the runner's `hostname -s`, and `host.locked_at`) and a
run elsewhere is refused before anything is built: `check` exits 4 with
the reason.  With --update-baseline a run on another machine is the
re-lock itself: it is let through, and `update` rewrites the floors from
it and moves `host` to this machine.

The competitor snapshot names the machine it was read on too; the vs-best
rows divide by it, so `competitor_mismatch` says when that is not this one.
"""
import datetime
import json
import math
import subprocess
import sys

sys.dont_write_bytecode = True  # no __pycache__ next to the scripts


def machine():
    """This machine's short host name, as the run record's `runner`."""
    return subprocess.run(["hostname", "-s"], capture_output=True, text=True).stdout.strip()


def locked_on(baseline):
    return (baseline.get("host") or {}).get("name")


def refusal(baseline, runner, update):
    """None when this run may be compared with the baseline, else why not."""
    locked = locked_on(baseline)
    if locked == runner:
        return None
    if update:
        print(f"==> re-locking the floors on {runner} (they were locked on {locked})")
        return None
    return (f"bench refused: the floors in bench/baseline.json were locked on {locked!r} "
            f"(host.locked_at {(baseline.get('host') or {}).get('locked_at')}), and this "
            f"run is on {runner!r}.  Readings from two machines are not compared.  Run "
            f"it on {locked}, or re-lock the floors here with --update-baseline.")


def competitor_mismatch(baseline, runner):
    snap = (baseline.get("competitors_snapshot") or {}).get("host") or {}
    name = snap.get("name")
    if name == runner:
        return None
    return (f"the other terminals were read on {name!r}, this run is on {runner!r}; "
            f"re-measure them here with bin/remote-measure-others.sh")


def update(baseline, baseline_path, rows, rc, runner):
    """--update-baseline: floors for the wall-clock and latency readings
    from this one run, with a safety margin.  The instructions-per-byte
    ceilings are not touched: they come from the spread over several
    runs, and one run cannot know it.  Nor are the size budgets: see
    bin/_size_budget.py.  On the floors' own machine the gate has to pass;
    on another machine its floors are not this machine's, so every gating
    row needs a valid reading instead."""
    relock = locked_on(baseline) != runner
    gating = [r for r in rows if r["gating"] and r["limit"] is not None]
    if relock and any(r["value"] is None or r["verdict"] in ("invalid", "skip") for r in gating):
        print(f"--update-baseline refused: a gating row has no valid reading on {runner}")
        return 1
    if not relock and rc != 0:
        print("--update-baseline refused: the gate is not passing")
        return 1
    print("==> updating floors: 7% headless, 10% live; instruction ceilings left alone")
    by = {r["metric"]: r["value"] for r in rows if r["value"] is not None}
    for entry in baseline["scenarios"]:
        short = entry["id"].removeprefix("cat-")
        if f"parse MB/s {short}" in by:
            entry["mars_parse_MBps_min"] = round(by[f"parse MB/s {short}"] * 0.93)
        # mars_vs_best_other_min is a requirement, not a reading: it is
        # never derived from a run
        live = by.get(f"app MB/s {short}")
        if live is not None and "mars_live_MBps_min" in entry:
            entry["mars_live_MBps_min"] = round(live * 0.90)
    # ceil, not round: 1.9 x 1.30 = 2.47 rounds to 2 and the margin is gone
    for label, key, margin in (("render p99", "render_full_repaint", 1.50),
                               ("scroll p99", "scroll_repaint", 1.30),
                               ("scroll-cold p99", "scroll_cold_repaint", 1.30)):
        if label in by and key in baseline:
            baseline[key]["p99_us_max"] = math.ceil(by[label] * margin)
    baseline["host"] = {"name": runner, "locked_at": datetime.date.today().isoformat()}
    with open(baseline_path, "w") as f:
        json.dump(baseline, f, indent=2)
        f.write("\n")
    print("==> wrote", baseline_path)
    return 0


def self_test():
    """The machine rule can refuse and can let through."""
    b = {"host": {"name": "a", "locked_at": "2026-11-25"},
         "competitors_snapshot": {"host": {"name": "a"}}}
    ok = (refusal(b, "a", False) is None and refusal(b, "b", False) is not None
          and refusal(b, "b", True) is None and refusal({}, "b", False) is not None
          and competitor_mismatch(b, "a") is None and competitor_mismatch(b, "b") is not None)
    print("SELF-TEST PASS" if ok else "SELF-TEST FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        sys.exit(self_test())
    if len(sys.argv) != 4 or sys.argv[1] != "check":
        print(__doc__.split("\n\n")[1], file=sys.stderr)
        sys.exit(2)
    why = refusal(json.load(open(sys.argv[2])), machine(), sys.argv[3] == "1")
    if why:
        print(why, file=sys.stderr)
        sys.exit(4)
