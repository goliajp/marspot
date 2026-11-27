#!/usr/bin/env python3
"""Which machine bench/baseline.json belongs to, and rewriting its floors.

  _bench_baseline.py check <baseline.json> <update 0|1> <fast|full>
  _bench_baseline.py --self-test

A wall-clock floor or a tail latency read on one machine says nothing
about another, so the baseline names the machine its floors were locked
on (`host.name`, the runner's `hostname -s`, and `host.locked_at`) and a
run elsewhere is refused before anything is built: `check` exits 4 with
the reason.  With --update-baseline --full a run on another machine is
the re-lock itself: it is let through, and `update` rewrites every limit
in RELOCK from it and moves `host` to this machine, or writes nothing
when one of them has no valid reading.  A fast run reads only part of
those limits, so it cannot re-lock: the rest would keep the old machine's
numbers under the new machine's name.

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


def refusal(baseline, runner, update, mode="full"):
    """None when this run may be compared with the baseline, else why not."""
    locked = locked_on(baseline)
    if locked == runner:
        return None
    if update and mode != "full":
        return (f"bench refused: re-locking the floors on {runner!r} (they were locked on "
                f"{locked!r}) needs --full --update-baseline.  A fast run reads only the "
                f"headless rows, and every other limit would keep {locked}'s numbers "
                f"labelled as {runner}'s.")
    if update:
        print(f"==> re-locking the floors on {runner} (they were locked on {locked})")
        return None
    return (f"bench refused: the floors in bench/baseline.json were locked on {locked!r} "
            f"(host.locked_at {(baseline.get('host') or {}).get('locked_at')}), and this "
            f"run is on {runner!r}.  Readings from two machines are not compared.  Run "
            f"it on {locked}, or re-lock the floors here with --full --update-baseline.")


def competitor_mismatch(baseline, runner):
    snap = (baseline.get("competitors_snapshot") or {}).get("host") or {}
    name = snap.get("name")
    if name == runner:
        return None
    return (f"the other terminals were read on {name!r}, this run is on {runner!r}; "
            f"re-measure them here with bin/remote-measure-others.sh")


# Every limit a re-lock rewrites: (where in the baseline, key, the row
# that reads it, how the limit follows from the row).  "min" floors are
# the reading x margin; "max" ceilings the reading x margin rounded up;
# "insn" ceilings the worst trial x margin, the rule the instruction
# notes in bench/baseline.json describe.  Requirements (vs-best ratios)
# and size budgets are not readings of a machine and are not here.
def relock_targets(baseline):
    for e in baseline.get("scenarios", []):
        short = e["id"].removeprefix("cat-")
        yield e, "mars_parse_MBps_min", f"parse MB/s {short}", "min", 0.93
        yield e, "mars_parse_file_MBps_min", f"parse-file MB/s {short}", "min", 0.93
        yield e, "mars_live_MBps_min", f"app MB/s {short}", "min", 0.90
        yield e, "mars_l3_MBps_min", f"L3 MB/s {short}", "min", 0.90
        yield e, "mars_parse_insn_per_byte_max", f"parse insn/byte {short}", "insn", 1.01
        yield e, "mars_parse_file_insn_per_byte_max", f"parse-file insn/byte {short}", "insn", 1.02
        yield e, "mars_l3_insn_per_byte_max", f"L3 insn/byte {short}", "insn", 1.02
    for key, label, margin in (("render_full_repaint", "render p99", 1.50),
                               ("scroll_repaint", "scroll p99", 1.30),
                               ("scroll_cold_repaint", "scroll-cold p99", 1.30)):
        yield baseline.get(key, {}), "p99_us_max", label, "max", margin
    kf = baseline.get("key_to_frame", {})
    yield kf, "p99_us_max", "key-to-frame p99", "max", 1.30
    yield kf, "p50_us_max", "key-to-frame p50", "max", 1.20
    m = baseline.get("multi_session_l3", {})
    if m:
        short = m.get("scenario", "").removeprefix("cat-")
        n = m.get("sessions")
        yield m, "insn_per_byte_max", f"L3x{n} insn/byte {short}", "insn", 1.05
        yield m, "aggregate_MBps_min", f"L3x{n} MB/s {short}", "min", 0.90
    yield baseline.get("app_footprint_12_panes", {}), "sum_KiB_max", "app footprint 12 panes", "max", 1.10
    st = baseline.get("app_startup", {})
    yield st, "cold_ms_max", "startup cold", "max", 1.30
    yield st, "warm_ms_max", "startup warm", "max", 1.30
    pc = baseline.get("pane_control", {})
    for k in [k for k in pc if k.endswith("_p95_ms_max")]:
        yield pc, k, f"pane control {k.removesuffix('_p95_ms_max')} p95", "max", 1.30


def relock_values(baseline, rows):
    """[(container, key, new value)] for every limit present, or the
    names of the limits with no valid reading."""
    by = {r["metric"]: r for r in rows if r.get("value") is not None}
    out, missing = [], []
    for c, key, label, kind, margin in relock_targets(baseline):
        if key not in c:
            continue
        r = by.get(label)
        if r is None:
            missing.append(f"{key} ({label})")
            continue
        if kind == "min":
            v = round(r["value"] * margin, 1)
        elif kind == "insn":
            v = round(max(r["trials"] or [r["value"]]) * margin, 2)
        elif isinstance(c[key], int):
            v = math.ceil(r["value"] * margin)
        else:
            v = math.ceil(round(r["value"] * margin * 10, 6)) / 10
        out.append((c, key, v))
    return out, missing


def update(baseline, baseline_path, rows, rc, runner, mode):
    """--update-baseline.  On the floors' own machine the gate has to
    pass, and only the floors for the wall-clock and latency readings
    move, from this one run with a safety margin: the instruction
    ceilings come from the spread over several runs, which one run cannot
    know, and the size budgets are bin/_size_budget.py's.  On another
    machine nothing of the old one may stay: a full run, every limit in
    relock_targets() rewritten, or nothing written."""
    if locked_on(baseline) != runner:
        return relock(baseline, baseline_path, rows, runner, mode)
    if rc != 0:
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
    return write(baseline, baseline_path, runner)


def relock(baseline, baseline_path, rows, runner, mode):
    if mode != "full":
        print(f"--update-baseline refused: re-locking on {runner} needs --full")
        return 1
    values, missing = relock_values(baseline, rows)
    if missing:
        print(f"--update-baseline refused: re-locking on {runner} rewrites every limit, and "
              f"{len(missing)} have no valid reading in this run: " + ", ".join(missing))
        return 1
    print(f"==> re-locking {len(values)} limits on {runner} from this run")
    for c, key, v in values:
        c[key] = v
    return write(baseline, baseline_path, runner)


def write(baseline, baseline_path, runner):
    baseline["host"] = {"name": runner, "locked_at": datetime.date.today().isoformat()}
    with open(baseline_path, "w") as f:
        json.dump(baseline, f, indent=2)
        f.write("\n")
    print("==> wrote", baseline_path)
    return 0


def self_test():
    """The machine rule can refuse and can let through, and a re-lock
    writes every limit or none."""
    b = {"host": {"name": "a", "locked_at": "2026-11-25"},
         "competitors_snapshot": {"host": {"name": "a"}}}
    ok = (refusal(b, "a", False) is None and refusal(b, "b", False) is not None
          and refusal(b, "b", True, "full") is None and refusal(b, "b", True, "fast") is not None
          and refusal(b, "a", True, "fast") is None and refusal({}, "b", False) is not None
          and competitor_mismatch(b, "a") is None and competitor_mismatch(b, "b") is not None)
    b.update(scenarios=[{"id": "cat-ascii", "mars_parse_MBps_min": 100.0,
                         "mars_l3_insn_per_byte_max": 40.0, "mars_vs_best_other_min": 1.0}],
             render_full_repaint={"p99_us_max": 1000}, pane_control={"send_p95_ms_max": 4.2})
    rows = [{"metric": "parse MB/s ascii", "value": 200.0, "trials": [199.0, 200.0]},
            {"metric": "L3 insn/byte ascii", "value": 50.0, "trials": [49.0, 51.0]},
            {"metric": "render p99", "value": 900.0, "trials": [900.0]},
            {"metric": "pane control send p95", "value": 5.0, "trials": [5.0]}]
    values, missing = relock_values(b, rows)
    got = {k: v for _, k, v in values}
    ok = ok and not missing and got == {"mars_parse_MBps_min": 186.0, "mars_l3_insn_per_byte_max": 52.02,
                                        "p99_us_max": 1350, "send_p95_ms_max": 6.5}
    values, missing = relock_values(b, rows[1:])
    ok = ok and missing == ["mars_parse_MBps_min (parse MB/s ascii)"]
    print("SELF-TEST PASS" if ok else "SELF-TEST FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        sys.exit(self_test())
    if len(sys.argv) != 5 or sys.argv[1] != "check":
        print(__doc__.split("\n\n")[1], file=sys.stderr)
        sys.exit(2)
    why = refusal(json.load(open(sys.argv[2])), machine(), sys.argv[3] == "1", sys.argv[4])
    if why:
        print(why, file=sys.stderr)
        sys.exit(4)
