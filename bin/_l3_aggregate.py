#!/usr/bin/env python3
"""Turns the trials of bin/measure-l3.sh into its result file.

  _l3_aggregate.py trial <run-dir> <scenario> <bytes-per-session> <sessions>
      judge the trial that just ran (probe.<i>.out, probe.<i>.err,
      trial.load, cpu.before, cpu.after in <run-dir>), append it to
      <run-dir>/<scenario>.trials and say what it read.
      exit 0 valid, 3 invalid (the host was busy), 4 failed (no timing)

  _l3_aggregate.py finish <run-dir> <out.json> <sessions> <scenario>...
      write the result file from the recorded trials

Units: MB = 10^6 bytes; instructions and cycles are those retired by the
marspot-session processes, summed over the sessions of a trial and
divided by the bytes they absorbed.
"""
import json
import os
import sys
import time

sys.dont_write_bytecode = True  # no __pycache__ next to the scripts
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _bench_record import FOREIGN_MAX, LOAD_MAX, foreign_cores  # noqa: E402

# A trial of one session is judged on every load sample taken while it
# ran and on what other processes used over it.  Trials of several
# sessions are their own load: the first is judged on the sample taken
# before it starts, the later ones only on what other processes used.
# fewer valid trials than this and the scenario has no reading
MIN_VALID = int(os.environ.get("MARSPOT_BENCH_MIN_VALID", "3"))

UNITS = {
    "insn_per_byte": "instructions retired per byte, all sessions",
    "cycles_per_byte": "CPU cycles per byte, all sessions",
    "MB_per_s": "10^6 bytes per second of wall clock, all sessions together",
    "wall_ns": "nanoseconds, the slowest session of the trial",
    "load1": "1-minute load average of the host",
    "foreign_cores": "cores kept busy by other processes during the trial",
    "bytes": "bytes absorbed by one trial, all sessions",
}

# Instructions per (wakeup per MiB), fitted over 164 single-session
# trials on 2026-10-02: a starved session is chopped into more rounds
# for the same bytes and each round costs this much.  Subtracting it
# leaves the per-byte cost.
ROUND_OVERHEAD = {
    "cat-ascii": 22.58, "cat-mixed": 19.69, "cat-cjk": 17.57, "cat-emoji": 18.87,
}
STARVED_WAKEUPS_PER_MIB = 1.5


def median(xs):
    s = sorted(xs)
    n = len(s)
    return s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2


def read_floats(path):
    if not os.path.exists(path):
        return []
    return [float(x) for x in open(path).read().split()]


def trial(run_dir, scenario, bytes_per_session, sessions):
    probes = []
    for i in range(sessions):
        fields = open(os.path.join(run_dir, f"probe.{i}.out")).read().split()
        if len(fields) != 6 or not fields[0].isdigit() or fields[0] == "0":
            probes = None
            break
        witness = {}
        for line in open(os.path.join(run_dir, f"probe.{i}.err"), errors="replace"):
            if line.startswith("witness: "):
                witness = {
                    k: int(v)
                    for k, v in (kv.split("=", 1) for kv in line.split()[1:])
                    if v.isdigit()
                }
        probes.append({
            "wall_ns": int(fields[0]),
            "counters": None if fields[1] == "-" else [int(x) for x in fields[1:]],
            "witness": witness,
        })
    loads = read_floats(os.path.join(run_dir, "trial.load"))
    before = json.load(open(os.path.join(run_dir, "cpu.before")))
    after = json.load(open(os.path.join(run_dir, "cpu.after")))
    rec = {
        "load1_samples": loads,
        "foreign_cores": foreign_cores(before, after),
        "sessions": sessions,
    }
    if probes is None:
        rec["status"] = "failed"
    else:
        total = bytes_per_session * sessions
        wall = max(p["wall_ns"] for p in probes)
        rec.update({
            "bytes": total,
            "wall_ns": wall,
            "session_wall_ns": [p["wall_ns"] for p in probes],
            "MB_per_s": round(total / (wall / 1e9) / 1e6, 2),
            "witness": [p["witness"] for p in probes],
        })
        if all(p["counters"] for p in probes):
            user, sys_, wk, insn, cyc = (
                sum(p["counters"][j] for p in probes) for j in range(5)
            )
            rec.update({
                "user_ns": user, "sys_ns": sys_, "wakeups": wk,
                "instructions": insn, "cycles": cyc,
                "insn_per_byte": round(insn / total, 3),
                "cycles_per_byte": round(cyc / total, 3),
            })
        if sessions == 1:
            judged = loads
        elif os.path.exists(os.path.join(run_dir, f"{scenario}.trials")):
            judged = []  # the load is this scenario's own earlier trials
        else:
            judged = loads[:1]
        why = []
        if judged and max(judged) > LOAD_MAX:
            why.append(f"load1 {max(judged)} > {LOAD_MAX}")
        if rec["foreign_cores"] is not None and rec["foreign_cores"] > FOREIGN_MAX:
            why.append(f"other processes used {rec['foreign_cores']} cores > {FOREIGN_MAX}")
        if "insn_per_byte" not in rec:
            why.append("the kernel reported no counters")
        rec["status"] = "invalid" if why else "valid"
        if why:
            rec["invalid_because"] = "; ".join(why)
    with open(os.path.join(run_dir, f"{scenario}.trials"), "a") as f:
        f.write(json.dumps(rec) + "\n")
    load_s = f"load1 {min(loads)}-{max(loads)}" if loads else "load1 ?"
    if rec["status"] == "failed":
        print("    trial failed (no timing)")
        return 4
    print(
        f"    {rec['wall_ns'] / 1e9:.3f} s  {rec['MB_per_s']:.1f} MB/s  "
        f"{rec.get('insn_per_byte', '-')} insn/byte  "
        f"{rec.get('cycles_per_byte', '-')} cycles/byte  "
        f"{load_s}  others {rec['foreign_cores']} cores"
    )
    if rec["status"] == "invalid":
        print(f"    INVALID, not counted: {rec['invalid_because']}")
        return 3
    return 0


def finish(run_dir, out_json, sessions, scenarios):
    out = {"_units": UNITS, "_sessions": sessions,
           "_load_max": LOAD_MAX, "_foreign_cores_max": FOREIGN_MAX}
    all_loads = []
    for scenario in scenarios:
        path = os.path.join(run_dir, f"{scenario}.trials")
        if not os.path.exists(path):
            continue
        trials = [json.loads(x) for x in open(path) if x.strip()]
        valid = [t for t in trials if t["status"] == "valid"]
        loads = [x for t in trials for x in t["load1_samples"]]
        all_loads += loads
        entry = {
            "sessions": sessions,
            "trials_run": len(trials),
            "trials_valid": len(valid),
            "trials_invalid": sum(1 for t in trials if t["status"] == "invalid"),
            "trials_failed": sum(1 for t in trials if t["status"] == "failed"),
            "load1_start": loads[0] if loads else None,
            "load1_max": max(loads) if loads else None,
            "load1_end": loads[-1] if loads else None,
            "foreign_cores_max": max(
                (t["foreign_cores"] for t in trials if t["foreign_cores"] is not None),
                default=None,
            ),
            "trials": trials,
        }
        if len(valid) < MIN_VALID:
            reasons = sorted({t.get("invalid_because", t["status"]) for t in trials
                              if t["status"] != "valid"})
            entry["invalid"] = (
                f"{len(valid)} valid trial(s) of {len(trials)}, need {MIN_VALID}: "
                + " / ".join(reasons)
            )
            out[scenario] = entry
            continue
        ipb = [t["insn_per_byte"] for t in valid]
        cpb = [t["cycles_per_byte"] for t in valid]
        rate = [t["MB_per_s"] for t in valid]
        walls = sorted(t["wall_ns"] for t in valid)
        total = valid[0]["bytes"]
        entry.update({
            "bytes": total,
            # One session: the median.  A trial's count moves by a few
            # percent in both directions with how the session's reads
            # happened to be chopped up; over eight runs of one build
            # the median of seven trials stayed within 0.2-1.8 % and
            # their minimum within 0.2-4.6 %.
            # Several sessions: the minimum.  They get in each other's
            # way by a different amount every trial (213-386 for one
            # build), contention only adds work, and over the same
            # eight runs the minimum stayed within 8.6 % where the
            # median moved 35 %.
            "insn_per_byte": round(median(ipb) if sessions == 1 else min(ipb), 3),
            "insn_per_byte_stat": f"{'median' if sessions == 1 else 'min'}-of-{len(ipb)}",
            "insn_per_byte_samples": ipb,
            "cycles_per_byte": round(median(cpb), 3),
            "cycles_per_byte_samples": cpb,
            # Wall clock keeps the fastest trial: company can only add
            # time, so the fastest is the least disturbed.
            "MB_per_s": max(rate),
            "MB_per_s_stat": f"best-of-{len(rate)}",
            "MB_per_s_median": round(median(rate), 2),
            "MB_per_s_samples": rate,
            # the names older consumers read: best wall time and its rate
            "median_ns": walls[0],
            "stat": f"min-of-{len(walls)}",
            "bytes_per_sec": total * 1_000_000_000 // walls[0],
            "samples": walls,
        })
        if sessions == 1:
            mib = total / 1048576
            per_mib = [t["wakeups"] / mib for t in valid]
            entry["wakeups_per_mib"] = [round(x, 3) for x in per_mib]
            entry["starved_trials"] = sum(1 for x in per_mib if x > STARVED_WAKEUPS_PER_MIB)
            b = ROUND_OVERHEAD.get(scenario)
            if b:
                norm = [round(ipb[i] - b * per_mib[i], 2) for i in range(len(valid))]
                entry["insn_per_byte_normalised"] = norm
                entry["insn_per_byte_normalised_median"] = round(median(norm), 2)
        out[scenario] = entry
    out["_host_load1"] = max(all_loads) if all_loads else None
    out["_host_load1_first"] = all_loads[0] if all_loads else None
    out["_host_load1_last"] = all_loads[-1] if all_loads else None
    out["_host_load1_trials"] = len(all_loads)
    with open(out_json, "w") as f:
        json.dump(out, f, indent=2)
        f.write("\n")


def table(path):
    doc = json.load(open(path))
    print(f"{'scenario':<10} {'insn/byte':>9} {'trials':>15} {'MB/s':>7} {'trials':>13} "
          f"{'load1 start/max/end':>20} {'valid':>6}")
    for sid, e in doc.items():
        if sid.startswith("_"):
            continue
        load = f"{e['load1_start']}/{e['load1_max']}/{e['load1_end']}"
        n = f"{e['trials_valid']}/{e['trials_run']}"
        if "invalid" in e:
            print(f"{sid:<10} {'INVALID':>9} {'':>15} {'':>7} {'':>13} {load:>20} {n:>6}"
                  f"  {e['invalid']}")
            continue
        i, r = e["insn_per_byte_samples"], e["MB_per_s_samples"]
        print(f"{sid:<10} {e['insn_per_byte']:>9.2f} {f'{min(i):.2f}-{max(i):.2f}':>15} "
              f"{e['MB_per_s']:>7.1f} {f'{min(r):.1f}-{max(r):.1f}':>13} {load:>20} {n:>6}")


def settle(limit_s, below):
    """Wait, up to limit_s, for the load average to come under `below`.

    The load average is a one-minute mean, so it still carries whatever
    ran before: a build, the previous gate run, the previous trial of
    nine sessions.  Measured without this, a run started right after a
    nine-session trial threw away every one of its own trials as "the
    host was busy" when the host was only remembering the measurement.
    """
    deadline = time.time() + limit_s
    waited = False
    while os.getloadavg()[0] > below and time.time() < deadline:
        waited = True
        time.sleep(2)
    if waited:
        print(f"    waited for load1 to come under {below}: now {os.getloadavg()[0]:.2f}")


def main(argv):
    if len(argv) >= 6 and argv[1] == "trial":
        return trial(argv[2], argv[3], int(argv[4]), int(argv[5]))
    if len(argv) >= 6 and argv[1] == "finish":
        finish(argv[2], argv[3], int(argv[4]), argv[5:])
        return 0
    if len(argv) == 3 and argv[1] == "table":
        table(argv[2])
        return 0
    if len(argv) == 4 and argv[1] == "settle":
        settle(int(argv[2]), float(argv[3]))
        return 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
