#!/usr/bin/env python3
"""What a measurement was taken on, and where its history is kept.

Used by bin/bench.sh, bin/measure-l3.sh and bin/bench-remote.sh:

  _bench_record.py meta <root>            the tree, toolchain and host, as JSON
  _bench_record.py cpu-snapshot           CPU seconds of every process, as JSON
  _bench_record.py foreign <before> <after>
                                          cores other processes kept busy
                                          between two snapshots
  _bench_record.py time-l <stdout> <stderr>
                                          rate and instructions per byte of
                                          one `--bench parse` run made under
                                          `/usr/bin/time -l`
  _bench_record.py judge <dir> <block>    was the host quiet while <block>
                                          ran; writes <dir>/<block>.host,
                                          exit 3 when it was not
  _bench_record.py append <gate-record.json> [history-dir]
                                          one line per metric into
                                          <history-dir>/<metric>.jsonl
  _bench_record.py show [metric-substring] [-n N] [history-dir]
                                          the last N readings of each metric

Throughput is MB/s with MB = 10^6 bytes, everywhere.
"""
import json
import os
import re
import subprocess
import sys
import time

# readings kept per metric; a gate run adds one line to each file
KEEP = int(os.environ.get("MARSPOT_BENCH_HISTORY_KEEP", "500"))

# A reading is thrown away when the 1-minute load average passes LOAD_MAX
# at any sample taken while it was made, or when processes other than
# the measurement kept more than FOREIGN_MAX cores busy over it.  The
# load average includes what the measurement itself adds (one runnable
# thread for a headless parse, two to three per session for the live
# path), which is why the line is not lower.
LOAD_MAX = float(os.environ.get("MARSPOT_BENCH_LOAD_MAX", "5.0"))
FOREIGN_MAX = float(os.environ.get("MARSPOT_BENCH_FOREIGN_MAX", "1.0"))


def _out(args):
    return subprocess.run(args, capture_output=True, text=True).stdout.strip()


def meta(root):
    """The tree, the toolchain and the host a reading belongs to.

    The copy on a remote host has no repository, so the side that made
    the copy passes the commit and dirty flag through the environment.
    """
    commit = os.environ.get("MARSPOT_BENCH_COMMIT") or _out(
        ["git", "-C", root, "rev-parse", "HEAD"]
    )
    dirty_env = os.environ.get("MARSPOT_BENCH_DIRTY")
    if dirty_env in ("0", "1"):
        dirty = dirty_env == "1"
    elif commit:
        dirty = bool(_out(["git", "-C", root, "status", "--porcelain"]))
    else:
        dirty = None
    return {
        "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "commit": commit or None,
        "dirty": dirty,
        "rustc": _out(["rustc", "--version"]) or None,
        "runner": _out(["hostname", "-s"]) or None,
        "os": _out(["sw_vers", "-productVersion"]) or None,
    }


def cpu_snapshot():
    """CPU seconds consumed so far by every process, keyed by pid, and
    each one's command name."""
    procs, names = {}, {}
    for line in _out(["ps", "-A", "-o", "pid=,time=,comm="]).splitlines():
        pid, cputime, comm = line.split(None, 2)
        secs = 0.0
        for part in cputime.split(":"):
            secs = secs * 60 + float(part)
        procs[pid] = secs
        names[pid] = os.path.basename(comm)
    return {"t": time.time(), "procs": procs, "names": names}


def foreign_cores(before, after):
    """Cores kept busy, between two snapshots, by processes alive at both.

    What a measurement starts after the first snapshot and reaps before
    the second is in neither, so this is everything except the
    measurement itself.  A process that started or ended inside the
    window is not counted: read it next to the load average, which sees
    those and cannot tell them from the measurement.
    """
    elapsed = after["t"] - before["t"]
    if elapsed <= 0:
        return None
    busy = 0.0
    for pid, secs in after["procs"].items():
        # a pid that went backwards was reused
        if pid in before["procs"] and secs > before["procs"][pid]:
            busy += secs - before["procs"][pid]
    return round(busy / elapsed, 3)


def foreign_detail(before, after):
    """Who the foreign CPU was: the busiest other process, and sysmond.

    sysmond is named because it is what answers every `ps` on the host:
    processes queued behind the bench lock poll with it, and four of
    them once kept it at a fifth of a core through a measurement.
    """
    elapsed = after["t"] - before["t"]
    if elapsed <= 0:
        return {}
    used = {
        pid: secs - before["procs"][pid]
        for pid, secs in after["procs"].items()
        if pid in before["procs"] and secs > before["procs"][pid]
    }
    names = after.get("names", {})
    out = {"sysmond_cores": round(
        sum(v for pid, v in used.items() if names.get(pid) == "sysmond") / elapsed, 3)}
    if used:
        top = max(used, key=used.get)
        out["foreign_top"] = f"{names.get(top, '?')} {used[top] / elapsed:.3f}"
    return out


def lock_waiters():
    """Processes queued behind the host's bench lock, or None where
    there is no such lock.  Asked once per block, not per trial: the
    answer costs a `ps`, which is the noise being counted."""
    tool = "/usr/local/bin/bench-lock"
    if not os.path.exists(tool):
        return None
    return sum(1 for line in _out([tool, "status"]).splitlines() if "\twaiting " in line)


def time_l(out_path, err_path):
    """(bytes per second, instructions per byte) of one `--bench parse`
    run.  The count is the whole process's, start-up included; the bytes
    are what the bench fed."""
    doc = json.load(open(out_path))
    insn = None
    for line in open(err_path):
        if "instructions retired" in line:
            insn = int(line.split()[0])
    if insn is None:
        raise SystemExit(f"no 'instructions retired' in {err_path}")
    return doc["bytes_per_sec"], insn / (doc["bytes"] * doc["repeat"])


def judge(cur_dir, block):
    """Whether the host stayed quiet between <block>.begin and
    <block>.end, from the load samples and the two CPU snapshots."""
    def read(name):
        return open(os.path.join(cur_dir, name)).read()
    begin, end = float(read(f"{block}.begin")), float(read(f"{block}.end"))
    loads = [
        float(load) for t, load in
        (line.split() for line in read("load.samples").splitlines())
        if begin <= float(t) <= end
    ]
    foreign = foreign_cores(
        json.loads(read(f"{block}.cpu0")), json.loads(read(f"{block}.cpu1"))
    )
    why = []
    if loads and max(loads) > LOAD_MAX:
        why.append(f"load1 {max(loads)} > {LOAD_MAX}")
    if foreign is not None and foreign > FOREIGN_MAX:
        why.append(f"other processes used {foreign} cores > {FOREIGN_MAX}")
    host = {
        "load1": {
            "start": loads[0] if loads else None,
            "max": max(loads) if loads else None,
            "end": loads[-1] if loads else None,
            "samples": len(loads),
        },
        "foreign_cores": foreign,
        "lock_waiters": lock_waiters(),
        "invalid": "; ".join(why) or None,
    }
    host.update(foreign_detail(
        json.loads(read(f"{block}.cpu0")), json.loads(read(f"{block}.cpu1"))))
    with open(os.path.join(cur_dir, f"{block}.host"), "w") as f:
        json.dump(host, f)
    if why:
        print(f"    {block}: the host was busy ({host['invalid']})")
        return 3
    return 0


def history_dir(root):
    """bench/results/history of the main checkout, so that a reading
    taken from a linked worktree outlives the worktree."""
    env = os.environ.get("MARSPOT_BENCH_HISTORY_DIR")
    if env:
        return env
    common = _out(
        ["git", "-C", root, "rev-parse", "--path-format=absolute", "--git-common-dir"]
    )
    base = os.path.dirname(common) if common else root
    return os.path.join(base, "bench", "results", "history")


def slug(metric):
    return re.sub(r"[^A-Za-z0-9._]+", "-", metric).strip("-")


def append(record_path, hist_dir):
    record = json.load(open(record_path))
    os.makedirs(hist_dir, exist_ok=True)
    shared = {k: v for k, v in record.items() if k != "rows"}
    for row in record["rows"]:
        path = os.path.join(hist_dir, slug(row["metric"]) + ".jsonl")
        lines = open(path).read().splitlines() if os.path.exists(path) else []
        lines.append(json.dumps({**shared, **row}, sort_keys=True))
        with open(path, "w") as f:
            f.write("\n".join(lines[-KEEP:]) + "\n")
    return len(record["rows"])


def show(hist_dir, needle, last):
    if not os.path.isdir(hist_dir):
        print(f"no history at {hist_dir}")
        return
    for name in sorted(os.listdir(hist_dir)):
        if not name.endswith(".jsonl") or needle not in name:
            continue
        rows = [json.loads(x) for x in open(os.path.join(hist_dir, name)) if x.strip()]
        print(f"{name[:-6]}  ({len(rows)} readings)")
        for r in rows[-last:]:
            commit = (r.get("commit") or "unknown")[:9] + ("*" if r.get("dirty") else "")
            load = r.get("load1") or {}
            value = r.get("value")
            value_s = "invalid" if value is None else f"{value:.2f}"
            print(
                f"  {r.get('ts', '?'):20} {commit:10} {value_s:>10} {r.get('unit', ''):9}"
                f" limit {r.get('limit')}  {r.get('verdict', ''):8}"
                f" load1 {load.get('start')}/{load.get('max')}/{load.get('end')}"
                f"  others {r.get('foreign_cores')} sysmond {r.get('sysmond_cores')}"
                f" waiters {r.get('lock_waiters')}"
                f"  n={r.get('trial_count')}  {r.get('runner')}"
            )


def main(argv):
    cmd = argv[1] if len(argv) > 1 else ""
    if cmd == "meta":
        print(json.dumps(meta(argv[2])))
    elif cmd == "cpu-snapshot":
        print(json.dumps(cpu_snapshot()))
    elif cmd == "foreign":
        print(foreign_cores(json.load(open(argv[2])), json.load(open(argv[3]))))
    elif cmd == "lock-waiters":
        print(lock_waiters())
    elif cmd == "time-l":
        bps, ipb = time_l(argv[2], argv[3])
        print(f"{bps} {ipb:.3f}")
    elif cmd == "judge":
        return judge(argv[2], argv[3])
    elif cmd == "append":
        root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        hist = argv[3] if len(argv) > 3 else history_dir(root)
        print(f"==> {append(argv[2], hist)} readings appended to {hist}")
    elif cmd == "show":
        root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        args = argv[2:]
        last = 10
        if "-n" in args:
            i = args.index("-n")
            last = int(args[i + 1])
            del args[i : i + 2]
        needle = args[0] if args else ""
        hist = args[1] if len(args) > 1 else history_dir(root)
        show(hist, needle, last)
    else:
        print(__doc__, file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
