#!/usr/bin/env python3
"""Turns the markers of bin/_remote-measure-multi-mini.sh into its JSON.

  _multi_aggregate.py <session-dir> <windows> <rounds> <trials> <skew-s> "<terms>" <scenario>...

A window's marker holds, per trial, `==SCN== <id>`, `cpr=<row>,<col>`
and `span <shared-start> <start> <end>` (wall clock, seconds).  The
k-th trial of a scenario is the same trial in every window.

Units: MB = 10^6 bytes; a figure is the bytes of all windows over the
time from the earliest start to the latest end.
"""
import glob
import json
import os
import platform
import re
import subprocess
import sys

sys.dont_write_bytecode = True

LIVE_MIN_BYTES = int(os.environ.get("LIVE_MIN_BYTES", 32 * 1024 * 1024))
SENTINEL = os.environ.get("LIVE_SENTINEL_CPR", "7,11")
LOAD_MAX = float(os.environ.get("MARSPOT_BENCH_LOAD_MAX", "5.0"))
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def median(xs):
    s = sorted(xs)
    n = len(s)
    return s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2


def read_marker(path):
    """grid, {scenario: [(certified, shared, start, end), ...]}"""
    grid, trials = None, {}
    current, certified = None, False
    for line in open(path, errors="replace"):
        line = line.strip()
        m = re.match(r"^==GRID== (\d+) (\d+)", line)
        if m:
            grid = f"{m.group(2)}x{m.group(1)}"
            continue
        m = re.match(r"^==SCN== (\S+)", line)
        if m:
            current, certified = m.group(1), False
            continue
        m = re.match(r"^cpr=(.*)$", line)
        if m:
            certified = m.group(1) == SENTINEL
            continue
        m = re.match(r"^span (\S+) (\S+) (\S+)$", line)
        if m and current:
            trials.setdefault(current, []).append(
                (certified, float(m.group(1)), float(m.group(2)), float(m.group(3))))
    return grid, trials


def scenario_bytes(scenario):
    size = os.path.getsize(f"{ROOT}/bench/scenarios/{scenario}.bin")
    return size * max(1, -(-LIVE_MIN_BYTES // size))


def one_round(prefix, windows, trials, skew, scenarios):
    markers = [read_marker(p) for p in sorted(glob.glob(prefix + ".[0-9]*"))]
    rd = {"windows_up": len(markers)}
    grids = sorted({g for g, _ in markers if g})
    if grids:
        rd["grid"] = grids[0] if len(grids) == 1 else grids
    if os.path.exists(prefix + ".load"):
        rd["load1"] = [float(x) for x in open(prefix + ".load").read().split()]
        if max(rd["load1"]) > LOAD_MAX:
            rd["polluted"] = True
    if os.path.exists(prefix + ".cpu"):
        before, after = (float(x) for x in open(prefix + ".cpu").read().split())
        every = sum(scenario_bytes(s) for s in scenarios) * trials * windows
        # of every process of the terminal, for all the round's bytes
        rd["cpu_ms_per_128MiB"] = round((after - before) * 1000 * (128 << 20) / every, 1)
    for s in scenarios:
        total = scenario_bytes(s) * windows
        readings, dropped = [], []
        for k in range(trials):
            spans = [t[s][k] for _, t in markers if len(t.get(s, [])) > k]
            if len(spans) < windows:
                dropped.append(f"trial {k + 1}: {len(spans)} of {windows} windows finished")
                continue
            if not all(c for c, _, _, _ in spans):
                dropped.append(f"trial {k + 1}: a window's reply was not the sentinel position")
                continue
            late = max(st - shared for _, shared, st, _ in spans)
            if late > skew:
                dropped.append(f"trial {k + 1}: a window started {late * 1000:.0f} ms late")
                continue
            took = max(e for _, _, _, e in spans) - min(st for _, _, st, _ in spans)
            readings.append(round(total / took / 1e6, 1))
        if readings:
            rd[f"{s}_MBps"] = median(readings)
            rd[f"{s}_trials_MBps"] = readings
        if dropped:
            rd[f"{s}_dropped"] = dropped
    return rd


def version_of(term):
    if term == "marspot":
        out = subprocess.run(["bin/drivers/marspot.sh", "app-version"], cwd=ROOT,
                             capture_output=True, text=True).stdout.split("\n")
        commit = os.environ.get("MARSPOT_BENCH_COMMIT", "")
        return "; ".join(x.strip() for x in out if x.strip()) + (f"; git {commit}" if commit else "")
    plist = {"ghostty": "/Applications/Ghostty.app/Contents/Info.plist"}.get(term)
    if not plist or not os.path.exists(plist):
        return None
    def read(key):
        try:
            return subprocess.check_output(["defaults", "read", plist, key], text=True,
                                           stderr=subprocess.DEVNULL).strip()
        except Exception:
            return ""
    return f"{read('CFBundleShortVersionString')} (build {read('CFBundleVersion')})"


def main(argv):
    session, windows, rounds, trials, skew = argv[1], int(argv[2]), int(argv[3]), int(argv[4]), float(argv[5])
    terms, scenarios = argv[6].split(), argv[7:]
    sw = subprocess.run(["sw_vers"], capture_output=True, text=True).stdout
    os_line = " ".join(l.split(":", 1)[1].strip() for l in sw.splitlines() if ":" in l)
    out = {"windows": windows, "unit": "MB/s, all windows together",
           "host": {"name": platform.node().split(".")[0], "os": os_line}}
    for t in terms:
        rds = []
        for r in range(1, rounds + 1):
            rd = one_round(f"{session}/m-{t}-r{r}", windows, trials, skew, scenarios)
            rd["round"] = r
            rds.append(rd)
        rec = {"version": version_of(t), "rounds": rds}
        # the first of three or more rounds is a warm-up; a round taken
        # under load is not a reading, unless that is all there is
        used = rds[1:] if len(rds) >= 3 else rds
        clean = [rd for rd in used if not rd.get("polluted")]
        for s in scenarios:
            vals = [rd[f"{s}_MBps"] for rd in (clean or used) if f"{s}_MBps" in rd]
            if vals:
                rec[f"{s}_MBps"] = median(vals)
                rec[f"{s}_MBps_range"] = [min(vals), max(vals)]
        cpu = [rd["cpu_ms_per_128MiB"] for rd in (clean or used) if "cpu_ms_per_128MiB" in rd]
        if cpu:
            rec["cpu_ms_per_128MiB"] = median(cpu)
            rec["cpu_ms_per_128MiB_range"] = [min(cpu), max(cpu)]
        grids = {json.dumps(rd.get("grid")) for rd in rds if rd.get("grid")}
        if len(grids) == 1:
            rec["grid"] = json.loads(grids.pop())
        out[t] = rec
    print(json.dumps(out, indent=2))


if __name__ == "__main__":
    main(sys.argv)
