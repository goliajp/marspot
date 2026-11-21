#!/usr/bin/env python3
"""The key-to-frame rows of bin/_bench_gate.py, and their self-test.

  _bench_latency.py --self-test

Reads the reports bin/measure-latency.sh leaves in <cur-dir>/latency
(one per sandbox app, written by bin/_latency_report.py).  With three or
more runs the first is a warm-up.  Each row is the median over runs of
that run's percentile of the time from the key event to the shell's
present call returning.

`--self-test` runs the whole gate on fabricated reports: the p99 row
passes with the ceiling doubled and fails with it halved, a key whose
frame came after the next key fails the late-keys row, and a
measurement that stopped part way is no reading.
"""
import glob
import json
import os
import re
import sys

sys.dont_write_bytecode = True

# the stage of bin/_latency_report.py the rows read
TOTAL = "TOTAL event -> present returned"


def read_runs(cur_dir):
    """(reports in run order, why the set is not a reading or None)."""
    d = os.path.join(cur_dir, "latency")
    rc_path = os.path.join(cur_dir, "latency.rc")
    if not os.path.exists(rc_path):
        return [], "the measurement did not run"
    rc, rounds = (int(x) for x in open(rc_path).read().split())
    # type-1.ship.r<n>.json; the same directory holds type-1.ship.r<n>.cpu.json
    runs = {}
    for p in glob.glob(os.path.join(d, "type-1.ship.r*.json")):
        m = re.search(r"\.r(\d+)\.json$", p)
        if m:
            runs[int(m.group(1))] = p
    try:
        reports = [json.load(open(runs[n])) for n in sorted(runs)]
    except ValueError as e:
        return [], f"a report is not JSON ({e})"
    if rc != 0:
        return reports, f"bin/measure-latency.sh exited {rc} (see measure-latency.log)"
    if len(reports) != rounds:
        return reports, f"{len(reports)} reports for {rounds} runs"
    for r in reports:
        if TOTAL not in r.get("all keys", {}):
            return reports, "a report has no key-to-present figures"
        lost = r["keys_sent"] - r["keys_followed_to_a_present"] - r["keys_overlapping_the_next"]
        if lost:
            return reports, f"{lost} of {r['keys_sent']} keys were never followed to a present"
    return reports, None


def add_rows(baseline, cur_dir, mode, row, median, block_host):
    kf = baseline.get("key_to_frame")
    if mode != "full" or not kf:
        return
    reports, broken = read_runs(cur_dir)
    used = reports[1:] if len(reports) >= 3 else reports
    host = block_host("latency")
    if broken and not host.get("invalid"):
        host = dict(host, invalid=broken)
    # a key whose frame came after the next key was pressed took over
    # 120 ms: the percentiles leave it out, so it is counted here
    late = [r.get("keys_overlapping_the_next", 0) for r in used]
    # no trials: one late key in one run is a failure, not a split verdict
    row("key-to-frame late keys", sum(late) if used else None, "keys", 0, True,
        host=host, stat="sum", note=f"per run {late}")
    for stat, key in (("p99", "p99_us_max"), ("p50", "p50_us_max")):
        xs = [r["all keys"][TOTAL][stat] for r in used] if not broken else []
        row(f"key-to-frame {stat}", median(xs) if xs else None, "µs", kf.get(key), True,
            gating=key in kf, trials=xs, host=host)


def self_test():
    import shutil
    import subprocess
    import tempfile

    here = os.path.dirname(os.path.abspath(__file__))
    src = json.load(open(os.path.join(here, "..", "bench", "baseline.json")))
    ceiling = src["key_to_frame"]["p99_us_max"]
    work = tempfile.mkdtemp(prefix="marspot-gate-selftest-")
    try:
        ok = True
        env = dict(os.environ, MARSPOT_BENCH_HISTORY_DIR=os.path.join(work, "history"))

        def case(label, p99s, scale, rc=0, late=0, want=("key-to-frame p99 ", "pass")):
            cur = os.path.join(work, f"cur-{label}")
            os.makedirs(os.path.join(cur, "latency"))
            for n, p99 in enumerate(p99s, 1):
                rep = {"keys_sent": 200, "keys_followed_to_a_present": 200 - late, "keys_overlapping_the_next": late,
                       "all keys": {TOTAL: {"p50": p99 / 2, "p99": p99}}}
                json.dump(rep, open(os.path.join(cur, "latency", f"type-1.ship.r{n}.json"), "w"))
                json.dump({}, open(os.path.join(cur, "latency", f"type-1.ship.r{n}.cpu.json"), "w"))
            open(os.path.join(cur, "latency.rc"), "w").write(f"{rc} 4\n")
            b = json.loads(json.dumps(src))
            b["key_to_frame"]["p99_us_max"] = round(ceiling * scale)
            bp = os.path.join(work, f"baseline-{label}.json")
            json.dump(b, open(bp, "w"))
            root = os.path.join(work, f"root-{label}")
            os.makedirs(root)
            out = subprocess.run([sys.executable, os.path.join(here, "_bench_gate.py"), bp, cur, "full", "0", root],
                                 env=env, capture_output=True, text=True).stdout
            line = [l for l in out.splitlines() if l.startswith(want[0])]
            print(f"{label}: " + (line[0] if line else f"no {want[0].strip()} row"))
            return bool(line) and want[1] in line[0]

        runs = (3 * ceiling, 0.8 * ceiling, 0.75 * ceiling, 0.7 * ceiling)
        ok &= case("ceiling doubled", runs, 2.0)
        ok &= case("ceiling halved", runs, 0.5, want=("key-to-frame p99 ", "FAIL"))
        ok &= case("a key over the gap", runs, 2.0, late=1, want=("key-to-frame late keys ", "FAIL"))
        ok &= case("measurement stopped", runs[:2], 2.0, rc=1, want=("key-to-frame p99 ", "INVALID"))
        print("SELF-TEST PASS" if ok else "SELF-TEST FAIL")
        return 0 if ok else 1
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    if sys.argv[1:] != ["--self-test"]:
        sys.exit(__doc__)
    sys.exit(self_test())
