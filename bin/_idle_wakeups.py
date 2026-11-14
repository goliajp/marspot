#!/usr/bin/env python3
"""Wake-ups and CPU of a set of processes over an idle window.

    bin/_idle_wakeups.py --pattern ERE [--window 30] [--readings 3] [--json OUT]
                         [--budget WAKEUPS_PER_SEC] [--busy-load LOAD1]

Reads the kernel's own per-process counters (`proc_pid_rusage`: timer
wake-ups, wake-ups that took the package out of idle, CPU time in
nanoseconds, footprint) and per-thread CPU time (`proc_pidinfo`) at both
ends of each window, for every process whose command line matches the
pattern.  No root: the processes have to be the caller's own.

Two things are read twice by different means so a dead probe cannot pass
for a quiet process: CPU time also comes from `ps`, and a process whose
counters went backwards or that disappeared fails the run.

Exit codes, one meaning each (bin/measure-idle.sh and
bin/measure-idle-marspot.sh use the same four):

    0  measured, and within the budget when one was given
    1  measured on a quiet host, and over the budget
    2  the instrument failed: there is no reading to judge (bad usage,
       no matching process, the process set changed, a counter ran
       backwards, the two CPU clocks disagree, or this script crashed)
    3  not measured: the host was busy, so the reading is printed and
       written but is no verdict

The host counts as busy when load1 went over --busy-load (default
IDLE_BUSY_LOAD, else 5.0, the line bin/bench-remote.sh draws for a host
that has company) at any sample of any window.  Company can make a
reading worse and never better, so a reading within its budget stands
whatever the load was; one over its budget, or one with no budget to
hold it to, is worth nothing next to someone else's work and comes back
3, to be taken again on a quiet host.  3 is not a pass and not a
regression.
"""
import argparse
import ctypes
import json
import os
import re
import subprocess
import sys
import time
import traceback

MEASURED, OVER_BUDGET, INSTRUMENT_FAILED, HOST_BUSY = 0, 1, 2, 3


def broken(why):
    """The instrument failed: say why and leave with no verdict."""
    print(f"idle-wakeups: {why}", file=sys.stderr)
    sys.exit(INSTRUMENT_FAILED)

libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)

RUSAGE_INFO_V4 = 4
PROC_PIDLISTTHREADS = 6
PROC_PIDTHREADINFO = 5

RUSAGE_FIELDS = [
    "user_time", "system_time", "pkg_idle_wkups", "interrupt_wkups", "pageins",
    "wired_size", "resident_size", "phys_footprint", "proc_start_abstime",
    "proc_exit_abstime", "child_user_time", "child_system_time",
    "child_pkg_idle_wkups", "child_interrupt_wkups", "child_pageins",
    "child_elapsed_abstime", "diskio_bytesread", "diskio_byteswritten",
    "cpu_time_qos_default", "cpu_time_qos_maintenance", "cpu_time_qos_background",
    "cpu_time_qos_utility", "cpu_time_qos_legacy", "cpu_time_qos_user_initiated",
    "cpu_time_qos_user_interactive", "billed_system_time", "serviced_system_time",
    "logical_writes", "lifetime_max_phys_footprint", "instructions", "cycles",
    "billed_energy", "serviced_energy", "interval_max_phys_footprint", "runnable_time",
]


class RusageV4(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [(f, ctypes.c_uint64) for f in RUSAGE_FIELDS]


class ThreadInfo(ctypes.Structure):
    _fields_ = [
        ("user_time", ctypes.c_uint64), ("system_time", ctypes.c_uint64),
        ("cpu_usage", ctypes.c_int32), ("policy", ctypes.c_int32),
        ("run_state", ctypes.c_int32), ("flags", ctypes.c_int32),
        ("sleep_time", ctypes.c_int32), ("curpri", ctypes.c_int32),
        ("priority", ctypes.c_int32), ("maxpriority", ctypes.c_int32),
        ("name", ctypes.c_char * 64),
    ]


class Timebase(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


_tb = Timebase()
libc.mach_timebase_info(ctypes.byref(_tb))


def abs_to_secs(t):
    return t * _tb.numer / _tb.denom / 1e9


def rusage(pid):
    r = RusageV4()
    if libc.proc_pid_rusage(pid, RUSAGE_INFO_V4, ctypes.byref(r)) != 0:
        return None
    return r


def threads(pid):
    buf = (ctypes.c_uint64 * 512)()
    n = libc.proc_pidinfo(pid, PROC_PIDLISTTHREADS, 0, buf, ctypes.sizeof(buf))
    out = {}
    for h in buf[: max(n, 0) // 8]:
        ti = ThreadInfo()
        got = libc.proc_pidinfo(pid, PROC_PIDTHREADINFO, ctypes.c_uint64(h), ctypes.byref(ti), ctypes.sizeof(ti))
        if got == ctypes.sizeof(ti):
            out[h] = (ti.name.decode(errors="replace"), (ti.user_time + ti.system_time) / 1e9)
    return out


def ps_rows(pattern):
    text = subprocess.run(["ps", "-Ao", "pid=,cputime=,rss=,command="], capture_output=True, text=True).stdout
    rows = {}
    for line in text.splitlines():
        parts = line.split(None, 3)
        if len(parts) < 4 or "_idle_wakeups" in parts[3] or not pattern.search(parts[3]):
            continue
        secs = 0.0
        for p in parts[1].split(":"):
            secs = secs * 60 + float(p)
        rows[int(parts[0])] = (secs, int(parts[2]), parts[3].split(" ")[0].rsplit("/", 1)[-1])
    return rows


def snapshot(pattern):
    snap = {}
    for pid, (ps_cpu, rss, name) in ps_rows(pattern).items():
        r = rusage(pid)
        if r is None:
            broken(f"cannot read counters of pid {pid} ({name}); not this user's process?")
        snap[pid] = {
            "name": name, "ps_cpu": ps_cpu, "rss_kib": rss,
            "cpu": abs_to_secs(r.user_time + r.system_time),
            "idle_wk": r.pkg_idle_wkups, "intr_wk": r.interrupt_wkups,
            "footprint": r.phys_footprint, "threads": threads(pid),
        }
    return snap


def load1():
    return float(subprocess.run(["sysctl", "-n", "vm.loadavg"], capture_output=True, text=True).stdout.split()[1])


def reading(pattern, window):
    a, t0, loads = snapshot(pattern), time.time(), [load1()]
    if not a:
        broken("no process matches the pattern")
    while time.time() - t0 < window:
        time.sleep(min(10.0, max(0.05, window - (time.time() - t0))))
        loads.append(load1())
    b, secs = snapshot(pattern), time.time() - t0
    if sorted(a) != sorted(b):
        broken(f"the process set changed during the window ({sorted(a)} -> {sorted(b)})")
    procs = []
    for pid in sorted(b):
        x, y = a[pid], b[pid]
        d = {k: y[k] - x[k] for k in ("cpu", "idle_wk", "intr_wk", "ps_cpu")}
        if min(d.values()) < 0:
            broken(f"a counter of pid {pid} ran backwards")
        # ps counts in hundredths of a second; the two clocks must agree to that
        if abs(d["cpu"] - d["ps_cpu"]) > 0.03 + 0.2 * d["ps_cpu"]:
            broken(f"pid {pid}: rusage says {d['cpu']:.3f}s of CPU, ps says {d['ps_cpu']:.2f}s")
        th = []
        for h, (name, cpu) in y["threads"].items():
            before = x["threads"].get(h, (name, 0.0))[1]
            th.append({"name": name or "(unnamed)", "cpu_pct": round(100 * (cpu - before) / secs, 4)})
        th.sort(key=lambda t: -t["cpu_pct"])
        procs.append({
            "pid": pid, "name": y["name"],
            "cpu_pct": round(100 * d["cpu"] / secs, 4),
            "intr_wakeups_per_sec": round(d["intr_wk"] / secs, 2),
            "idle_wakeups_per_sec": round(d["idle_wk"] / secs, 2),
            "rss_kib": y["rss_kib"], "footprint_kib": y["footprint"] // 1024,
            "threads": th,
        })
    return {
        "window_secs": round(secs, 2), "load1_first": loads[0], "load1_max": max(loads),
        "cpu_pct": round(sum(p["cpu_pct"] for p in procs), 4),
        "intr_wakeups_per_sec": round(sum(p["intr_wakeups_per_sec"] for p in procs), 2),
        "idle_wakeups_per_sec": round(sum(p["idle_wakeups_per_sec"] for p in procs), 2),
        "rss_mib": round(sum(p["rss_kib"] for p in procs) / 1024, 1),
        "footprint_mib": round(sum(p["footprint_kib"] for p in procs) / 1024, 1),
        "processes": procs,
    }


def by_name(r):
    groups = {}
    for p in r["processes"]:
        g = groups.setdefault(p["name"], {"n": 0, "cpu_pct": 0.0, "intr": 0.0, "idle": 0.0, "rss": 0, "fp": 0, "threads": {}})
        g["n"] += 1
        g["cpu_pct"] += p["cpu_pct"]
        g["intr"] += p["intr_wakeups_per_sec"]
        g["idle"] += p["idle_wakeups_per_sec"]
        g["rss"] += p["rss_kib"]
        g["fp"] += p["footprint_kib"]
        for t in p["threads"]:
            g["threads"][t["name"]] = g["threads"].get(t["name"], 0.0) + t["cpu_pct"]
    return groups


def verdict(readings, budget, busy_load):
    """The exit code for these readings and the line that says why."""
    worst = max(r["intr_wakeups_per_sec"] for r in readings)
    load = max(r["load1_max"] for r in readings)
    if budget is not None and worst <= budget:
        return MEASURED, f"PASS: at most {worst:.1f} timer wake-ups/s, budget {budget}"
    if load > busy_load:
        held = "no budget to hold it to" if budget is None else f"budget {budget}"
        return HOST_BUSY, (f"NOT MEASURED: load1 reached {load} (busy over {busy_load}); {worst:.1f} timer wake-ups/s, "
                           f"{held}, read next to other work is no verdict")
    if budget is None:
        return MEASURED, ""
    return OVER_BUDGET, f"FAIL: {worst:.1f} timer wake-ups/s, budget {budget}"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pattern", required=True)
    ap.add_argument("--window", type=float, default=30.0)
    ap.add_argument("--readings", type=int, default=3)
    ap.add_argument("--json")
    ap.add_argument("--budget", type=float)
    ap.add_argument("--busy-load", type=float, default=float(os.environ.get("IDLE_BUSY_LOAD") or 5.0))
    ap.add_argument("--label", default="")
    args = ap.parse_args()
    pattern = re.compile(args.pattern)
    readings = []
    for i in range(args.readings):
        r = reading(pattern, args.window)
        readings.append(r)
        print(f"reading {i + 1}: {r['intr_wakeups_per_sec']:.1f} timer wake-ups/s ({r['idle_wakeups_per_sec']:.1f} from idle), "
              f"CPU {r['cpu_pct']:.3f}% of one core, RSS {r['rss_mib']} MiB, footprint {r['footprint_mib']} MiB, "
              f"{len(r['processes'])} processes, {r['window_secs']}s, load1 {r['load1_first']}..{r['load1_max']}")
        for name, g in sorted(by_name(r).items()):
            print(f"    {name:<18} x{g['n']:<3} {g['intr']:8.1f} wk/s  {g['idle']:8.1f} idle-wk/s  CPU {g['cpu_pct']:7.3f}%  "
                  f"RSS {g['rss'] // 1024} MiB  footprint {g['fp'] // 1024} MiB")
            busy = sorted(g["threads"].items(), key=lambda kv: -kv[1])[:6]
            print("        threads: " + ", ".join(f"{n} {c:.3f}%" for n, c in busy if c > 0))
    if args.json:
        with open(args.json, "w") as f:
            json.dump({"label": args.label, "pattern": args.pattern, "readings": readings}, f, indent=1)
            f.write("\n")
    code, line = verdict(readings, args.budget, args.busy_load)
    if line:
        print(line)
    sys.exit(code)


if __name__ == "__main__":
    try:
        main()
    except Exception:
        # an uncaught error would leave with 1, which says "over budget"
        traceback.print_exc()
        sys.exit(INSTRUMENT_FAILED)
