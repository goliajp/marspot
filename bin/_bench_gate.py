#!/usr/bin/env python3
"""The verdict half of bin/bench.sh.

  _bench_gate.py <baseline.json> <cur-dir> <fast|full> <update 0|1> <root>
  _bench_gate.py --self-test

The vs-best rows divide the product by the best other terminal in
`competitors_snapshot`.  A snapshot older than STALE_DAYS makes those
rows INCONCLUSIVE: a competitor reading from weeks ago is not a
comparison with the competitor as it is (Ghostty 1.3.1 to 1.4.0 moved
about 2.5x).  MARSPOT_BENCH_ALLOW_STALE_COMPETITORS=1 judges them
anyway, and the table says it did.  `--self-test` runs the gate on a
fabricated run twice, with the snapshot dated today and 15 days ago,
and checks that only the second makes the vs-best rows inconclusive.

Reads what bench.sh measured into <cur-dir>, judges it against the
baseline, prints the table, writes bench/results/gate-record.json and
appends one line per metric to the history (bin/_bench_record.py).

What gates and what does not.  A count of the work (instructions per
byte), a size or a tail latency gates.  A wall-clock throughput is
printed next to its floor and its spread and never decides the exit
code: one unchanged build read 101.7 to 139.8 MB/s across six runs of
one day while its instructions per byte stayed within 209 to 213.

Units: MB/s is 10^6 bytes per second in every column and every
baseline key.

Exit: 0 passed, 1 a gating row is over its limit, 3 nothing is over its
limit but at least one gating row has no valid reading.
"""
import json
import math
import os
import sys

sys.dont_write_bytecode = True  # no __pycache__ next to the scripts
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _bench_record  # noqa: E402
import _bench_latency  # noqa: E402

STALE_DAYS = 14


def self_test():
    """The staleness rule can fail and can pass: same run, two dates."""
    import datetime
    import shutil
    import subprocess
    import tempfile

    here = os.path.dirname(os.path.abspath(__file__))
    src = json.load(open(os.path.join(here, "..", "bench", "baseline.json")))
    work = tempfile.mkdtemp(prefix="marspot-gate-selftest-")
    try:
        cur = os.path.join(work, "cur")
        os.makedirs(cur)
        sids = [e["id"] for e in src["scenarios"] if "mars_vs_best_other_min" in e]
        rd = {f"{sid}_MBps": 100.0 for sid in sids}
        rd["load1"] = [1.0]
        json.dump({"marspot": {"rounds": [dict(rd), dict(rd), dict(rd)]}},
                  open(os.path.join(cur, "app.json"), "w"))
        env = {k: v for k, v in os.environ.items() if k != "MARSPOT_BENCH_ALLOW_STALE_COMPETITORS"}
        env["MARSPOT_BENCH_HISTORY_DIR"] = os.path.join(work, "history")
        seen = {}
        for label, age in (("fresh", 0), ("stale", STALE_DAYS + 1)):
            b = json.loads(json.dumps(src))
            day = datetime.date.today() - datetime.timedelta(days=age)
            b["competitors_snapshot"]["captured_at"] = day.isoformat()
            bp = os.path.join(work, f"baseline-{label}.json")
            json.dump(b, open(bp, "w"))
            root = os.path.join(work, f"root-{label}")
            os.makedirs(root)
            out = subprocess.run([sys.executable, os.path.abspath(__file__), bp, cur, "full", "0", root],
                                 env=env, capture_output=True, text=True).stdout
            vs = [l for l in out.splitlines() if l.startswith("vs-best ")]
            seen[label] = (vs, out)
        ok = True
        fresh, stale = seen["fresh"][0], seen["stale"][0]
        if len(stale) != len(sids) or len(fresh) != len(sids):
            print(f"SELF-TEST FAIL: expected {len(sids)} vs-best rows, got {len(fresh)} / {len(stale)}")
            print(seen["stale"][1][-2000:])
            ok = False
        if any("INCONCLUSIVE" in l for l in fresh):
            print("SELF-TEST FAIL: a snapshot dated today made a vs-best row inconclusive")
            ok = False
        if not stale or not all("INCONCLUSIVE" in l for l in stale):
            print(f"SELF-TEST FAIL: a snapshot {STALE_DAYS + 1} days old left a vs-best row judged")
            ok = False
        for l in stale:
            print(l)
        print("SELF-TEST PASS" if ok else "SELF-TEST FAIL")
        return 0 if ok else 1
    finally:
        shutil.rmtree(work, ignore_errors=True)


if sys.argv[1:] == ["--self-test"]:
    sys.exit(self_test())

baseline_path, cur_dir, mode, update_str, root = sys.argv[1:6]
do_update = update_str == "1"
baseline = json.load(open(baseline_path))
rows = []


def median(xs):
    s = sorted(xs)
    n = len(s)
    return s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2


def samples(name, cast=float):
    p = os.path.join(cur_dir, name)
    if not os.path.exists(p):
        return []
    return [cast(x) for x in open(p).read().split()]


def block_host(name):
    """Load and foreign CPU while a block of bench.sh ran, and whether
    that made it invalid."""
    p = os.path.join(cur_dir, f"{name}.host")
    return json.load(open(p)) if os.path.exists(p) else {}


def row(metric, value, unit, limit, lower_better, gating=True, trials=None,
        host=None, stat="median", note=None):
    """Judge one reading.  `host` is a block_host() dict; a block the
    host spoiled has no reading, whatever was measured in it."""
    host = host or {}
    trials = list(trials or [])
    r = {
        "metric": metric, "unit": unit, "limit": limit, "gating": gating,
        "limit_kind": "max" if lower_better else "min", "stat": stat,
        "trials": trials, "trial_count": len(trials),
        "load1": host.get("load1"), "foreign_cores": host.get("foreign_cores"),
        "sysmond_cores": host.get("sysmond_cores"), "lock_waiters": host.get("lock_waiters"),
    }
    if note:
        r["note"] = note
    if host.get("invalid"):
        r.update(value=None, verdict="invalid", note=host["invalid"])
    elif value is None:
        r.update(value=None, verdict="skip", note=note or "no measurement")
    else:
        r["value"] = value
        if limit is None:
            r["verdict"] = "info"
        else:
            ok = value <= limit if lower_better else value >= limit
            if ok:
                r["verdict"] = "pass"
            elif not gating:
                r["verdict"] = "below"
            # A failure the run's own trials do not agree on is not a
            # finding: unsure when more than a third of them are on the
            # passing side.  One trial on the passing side is not
            # enough -- with seven trials there nearly always is one,
            # and a change that added 5 % came back unsure on eight of
            # twelve rows while every median was over its ceiling.
            elif trials and 3 * sum(
                1 for t in trials if (t <= limit if lower_better else t >= limit)
            ) > len(trials):
                r["verdict"] = "unsure"
            else:
                r["verdict"] = "fail"
    rows.append(r)
    return r


def l3_doc(name):
    p = os.path.join(cur_dir, name)
    return json.load(open(p)) if os.path.exists(p) else None


def l3_host(entry):
    return {
        "load1": {"start": entry.get("load1_start"), "max": entry.get("load1_max"),
                  "end": entry.get("load1_end")},
        "foreign_cores": entry.get("foreign_cores_max"),
        "sysmond_cores": entry.get("sysmond_cores_max"),
        "lock_waiters": entry.get("lock_waiters"),
        "invalid": entry.get("invalid"),
    }


def best_other_mbps(sid):
    """Best competitor throughput for a scenario.  Excluded by design:
    terminal (the OS vendor's reference, not a competitor) and marspot
    (the subject)."""
    key = f"{sid}_MBps"
    # a row without "unit" predates the switch to 10^6 and is in 2^20
    vals = [
        v[key] * (1.0 if v.get("unit") == "MB/s" else 1.048576)
        for name, v in baseline["competitors_snapshot"].items()
        if isinstance(v, dict) and name not in ("terminal", "marspot") and key in v
    ]
    return max(vals) if vals else 0


# ---- the shipped path: one marspot-session per pane ----------------------
l3 = l3_doc("l3.json") if mode == "full" else None
for entry in baseline["scenarios"]:
    sid = entry["id"]
    short = sid.removeprefix("cat-")
    if l3 is not None and sid in l3:
        e = l3[sid]
        host = l3_host(e)
        row(f"L3 insn/byte {short}", e.get("insn_per_byte"), "insn/byte",
            entry.get("mars_l3_insn_per_byte_max"), True,
            trials=e.get("insn_per_byte_samples"), host=host)
        row(f"L3 cycles/byte {short}", e.get("cycles_per_byte"), "cycles/byte",
            None, True, gating=False, trials=e.get("cycles_per_byte_samples"), host=host)
        rate = e.get("MB_per_s")
        row(f"L3 MB/s {short}", rate, "MB/s", entry.get("mars_l3_MBps_min"), False,
            gating=False, trials=e.get("MB_per_s_samples"), host=host, stat="best")

# ---- the product in its window, against the best other terminal ----------
# Shell, core and one session with one pane, read with the trial script
# the snapshot's terminals were read with.  The requirement is that the
# product is the fastest.  On 2026-11-13 it is level with the best other
# terminal, inside the spread between rounds, so the ratio is printed
# on every run and does not yet decide the exit code.
app = (l3_doc("app.json") or {}).get("marspot") if mode == "full" else None
if mode == "full":
    rounds = (app or {}).get("rounds", [])
    used = [rd for rd in (rounds[1:] if len(rounds) >= 3 else rounds) if not rd.get("polluted")]
    loads = [x for rd in rounds for x in rd.get("load1", [])]
    host = {"load1": {"start": loads[0], "max": max(loads), "end": loads[-1]}} if loads else {}
    if not rounds:
        host["invalid"] = "the product produced no reading"
    elif not used:
        host["invalid"] = "every round was taken above the load ceiling"
    import datetime
    snap_day = baseline["competitors_snapshot"].get("captured_at")
    try:
        snap_age = (datetime.date.today() - datetime.date.fromisoformat(snap_day)).days
    except (TypeError, ValueError):
        snap_age = None
    allow_stale = os.environ.get("MARSPOT_BENCH_ALLOW_STALE_COMPETITORS") == "1"
    stale = None
    if snap_age is None:
        stale = f"the competitor snapshot has no readable captured_at ({snap_day!r})"
    elif snap_age > STALE_DAYS:
        stale = (f"the competitor snapshot is {snap_age} days old (> {STALE_DAYS}); "
                 f"re-measure the others with bin/remote-measure-others.sh")
    for entry in baseline["scenarios"]:
        sid = entry["id"]
        if "mars_vs_best_other_min" not in entry:
            continue
        short = sid.removeprefix("cat-")
        per_round = [rd[f"{sid}_MBps"] for rd in used if f"{sid}_MBps" in rd]
        rate = median(per_round) if per_round else None
        row(f"app MB/s {short}", rate, "MB/s", entry.get("mars_live_MBps_min"), False,
            gating=False, trials=per_round, host=host)
        other = best_other_mbps(sid)
        r = row(f"vs-best {short}", rate / other if rate and other > 0 else None, "ratio",
                entry["mars_vs_best_other_min"], False, gating=False,
                trials=[x / other for x in per_round] if other > 0 else [], host=host,
                note=None if rate else "the product produced no certified reading")
        if stale and allow_stale:
            r["note"] = f"{stale}; judged anyway (MARSPOT_BENCH_ALLOW_STALE_COMPETITORS=1)"
        elif stale and r["verdict"] not in ("invalid", "skip"):
            r.update(verdict="inconclusive", note=stale)

# ---- nine sessions absorbing the same scenario at once -------------------
multi = l3_doc("l3-multi.json") if mode == "full" else None
multi_cfg = baseline.get("multi_session_l3")
if multi is not None and multi_cfg:
    sid = multi_cfg["scenario"]
    e = multi.get(sid, {})
    host = l3_host(e)
    n = multi_cfg["sessions"]
    short = sid.removeprefix("cat-")
    row(f"L3x{n} insn/byte {short}", e.get("insn_per_byte"), "insn/byte",
        multi_cfg.get("insn_per_byte_max"), True,
        trials=e.get("insn_per_byte_samples"), host=host)
    row(f"L3x{n} cycles/byte {short}", e.get("cycles_per_byte"), "cycles/byte",
        None, True, gating=False, trials=e.get("cycles_per_byte_samples"), host=host)
    row(f"L3x{n} MB/s {short}", e.get("MB_per_s"), "MB/s",
        multi_cfg.get("aggregate_MBps_min"), False, gating=False,
        trials=e.get("MB_per_s_samples"), host=host, stat="best")

# ---- headless parse, in-RAM and file-backed scrollback -------------------
for entry in baseline["scenarios"]:
    sid = entry["id"]
    short = sid.removeprefix("cat-")
    for label, prefix, block, floor_key, insn_key in (
        ("parse", "parse", "parse", "mars_parse_MBps_min", "mars_parse_insn_per_byte_max"),
        ("parse-file", "parsefile", "parsefile", "mars_parse_file_MBps_min",
         "mars_parse_file_insn_per_byte_max"),
    ):
        rates = [x / 1e6 for x in samples(f"{prefix}-{sid}.samples")]
        insn = samples(f"{prefix}-{sid}.insn")
        if not rates:
            continue
        host = block_host(block)
        row(f"{label} insn/byte {short}", median(insn) if insn else None, "insn/byte",
            entry.get(insn_key), True, trials=insn, host=host)
        row(f"{label} MB/s {short}", median(rates), "MB/s", entry.get(floor_key), False,
            gating=False, trials=rates, host=host)

# ---- frame, scroll, size ------------------------------------------
for label, name, block, key in (
    ("render p99", "render.samples", "render", "render_full_repaint"),
    ("scroll p99", "scroll.samples", "scroll", "scroll_repaint"),
    ("scroll-cold p99", "scroll-cold.samples", "scroll", "scroll_cold_repaint"),
):
    xs = [x / 1000 for x in samples(name)]
    if xs and key in baseline:
        row(label, median(xs), "µs", baseline[key]["p99_us_max"], True,
            trials=xs, host=block_host(block))

for bin_name, ceiling in baseline.get("binary_size_bytes_max", {}).items():
    if bin_name.startswith("_"):
        continue
    xs = samples(f"size-{bin_name}.txt", int)
    # a ceiling with no reading is a check that did not run, not a pass
    row(f"size {bin_name}", xs[0] if xs else None, "bytes", ceiling, True, trials=xs,
        stat="exact", host=None if xs else {"invalid": f"no size reading for {bin_name}: not built?"})

_bench_latency.add_rows(baseline, cur_dir, mode, row, median, block_host)

# ---- the shipped app in a window: 12 panes' memory ------------------------
if mode == "full":
    fp = l3_doc("footprint.json")
    cfg = baseline.get("app_footprint_12_panes", {})
    if fp is None:
        host = {"invalid": "no reading: bin/measure-app-footprint.sh did not finish"}
    else:
        host = {"load1": {"start": fp["load1_start"], "max": fp["load1_max"], "end": fp["load1_end"]}}
        if not fp["all_idle"]:
            host["invalid"] = "a round was read before the processes went idle"
    row("app footprint 12 panes", fp["sum_bytes"] / 1024 if fp else None, "KiB",
        cfg.get("sum_KiB_max"), True, trials=[x / 1024 for x in (fp or {}).get("sum_bytes_samples", [])],
        host=host)

# ---- print ----------------------------------------------------------------
VERDICT = {
    "pass": "✓ pass", "fail": "✗ FAIL", "unsure": "? UNSURE", "skip": "- skip",
    "invalid": "! INVALID", "info": "  (not gated)", "below": "  below (not gated)",
    "inconclusive": "? INCONCLUSIVE",
}


def fmt(v, unit):
    if v is None:
        return "-"
    if unit in ("bytes", "KiB"):
        return f"{v:.0f}"
    return f"{v:.2f}" if unit in ("insn/byte", "cycles/byte", "ratio") else f"{v:.1f}"


print()
print("MB = 10^6 bytes.  insn/byte and cycles/byte: lower is better.")
header = (f"{'metric':<26} {'value':>10} {'unit':<11} {'limit':>12} {'trials min-max':>19} "
          f"{'load1 s/max/e':>15}   verdict")
print(header)
print("-" * len(header))
for r in rows:
    u = r["unit"]
    limit = "-" if r["limit"] is None else (
        ("≤" if r["limit_kind"] == "max" else "≥") + fmt(r["limit"], u))
    t = r["trials"]
    span = f"{fmt(min(t), u)}-{fmt(max(t), u)}" if len(t) > 1 else ""
    ld = r["load1"] or {}
    load = "/".join("-" if ld.get(k) is None else f"{ld[k]:.1f}" for k in ("start", "max", "end"))
    tail = f"  {r['note']}" if (r["verdict"] in ("invalid", "skip", "inconclusive")
                                or r["metric"].startswith("vs-best ")) and r.get("note") else ""
    print(f"{r['metric']:<26} {fmt(r['value'], u):>10} {u:<11} {limit:>12} {span:>19} "
          f"{load:>15}   {VERDICT[r['verdict']]}{tail}")
print()

gating = [r for r in rows if r["gating"] and r["limit"] is not None]
n_fail = sum(1 for r in gating if r["verdict"] == "fail")
n_open = sum(1 for r in gating if r["verdict"] in ("unsure", "invalid"))
n_below = sum(1 for r in rows if r["verdict"] == "below")
if n_fail:
    status, rc = "failed", 1
    print(f"GATE FAILED ({n_fail} regression(s), {n_open} without a verdict, "
          f"of {len(gating)} gating checks)")
elif n_open:
    status, rc = "inconclusive", 3
    print(f"GATE INCONCLUSIVE ({n_open} of {len(gating)} gating checks have no verdict: "
          f"the host was busy while they were measured, or this run's own trials fall on "
          f"both sides of the limit.  Run it again on a quiet machine.)")
else:
    status, rc = "passed", 0
    print(f"GATE PASSED ({len(gating)} gating checks)")
if n_below:
    print(f"{n_below} wall-clock reading(s) are below their floor; they do not gate.")
vs = [r for r in rows if r["metric"].startswith("vs-best ")]
if vs:
    cs = baseline["competitors_snapshot"]
    read_on = cs.get("captured_at", "an unknown day")
    versions = ", ".join(f"{n} {v['version']}" for n, v in cs.items()
                         if isinstance(v, dict) and n not in ("host", "marspot") and v.get("version"))
    read_on = f"{read_on}: {versions}" if versions else read_on
    print(f"PRODUCT vs BEST OTHER TERMINAL (rounds min-max; the others were read on {read_on}): " + ",  ".join(
        f"{r['metric'].removeprefix('vs-best ')} "
        + ("no reading" if r.get("value") is None else
           f"{r['value']:.2f}x ({min(r['trials']):.2f}-{max(r['trials']):.2f})")
        for r in vs))

# ---- record ---------------------------------------------------------------
record = _bench_record.meta(root)
record.update({
    "mode": mode, "gate": status,
    "units": {"MB/s": "10^6 bytes per second", "insn/byte": "instructions retired per byte",
              "cycles/byte": "CPU cycles per byte", "µs": "microseconds, p99",
              "KiB": "resident memory, 1024 bytes", "ratio": "marspot / best other terminal"},
    "load_max": _bench_record.LOAD_MAX, "foreign_cores_max": _bench_record.FOREIGN_MAX,
    "rows": rows,
})
results_dir = os.path.join(root, "bench", "results")
os.makedirs(results_dir, exist_ok=True)
record_path = os.path.join(results_dir, "gate-record.json")
with open(record_path, "w") as f:
    json.dump(record, f, indent=1)
    f.write("\n")
hist = _bench_record.history_dir(root)
print(f"==> {record_path}; {_bench_record.append(record_path, hist)} readings appended to {hist}")

# ---- --update-baseline -----------------------------------------------------
if do_update:
    if rc != 0:
        print("--update-baseline refused: the gate is not passing")
        sys.exit(1)
    # Floors for the wall-clock and latency readings, from this one run
    # with a safety margin.  The instructions-per-byte ceilings are not
    # touched: they come from the spread over several runs, and one run
    # cannot know it.
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
    for bin_name in baseline.get("binary_size_bytes_max", {}):
        if f"size {bin_name}" in by:
            baseline["binary_size_bytes_max"][bin_name] = round(by[f"size {bin_name}"] * 1.10)
    with open(baseline_path, "w") as f:
        json.dump(baseline, f, indent=2)
        f.write("\n")
    print("==> wrote", baseline_path)

sys.exit(rc)
