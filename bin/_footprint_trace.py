#!/usr/bin/env python3
"""The memory footprint of a process and its children over its first seconds.

    bin/_footprint_trace.py PID [--secs 12] [--every 0.02] [--label TEXT]

Reads `phys_footprint` (`proc_pid_rusage`) of PID and of each child it
has, by name, every `--every` seconds for `--secs`, and prints per
process: the peak, when it was reached, how long the footprint stayed
above half of the peak, and the last reading.  A one-shot reading taken
later cannot see a peak that has already gone.
"""
import argparse
import ctypes
import time

libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
RUSAGE_INFO_V4 = 4
FOOTPRINT_INDEX = 7  # after the uuid: the eighth u64 of rusage_info_v4


class Rusage(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_uint8 * 16), ("f", ctypes.c_uint64 * 35)]


def footprint(pid):
    r = Rusage()
    if libc.proc_pid_rusage(pid, RUSAGE_INFO_V4, ctypes.byref(r)) != 0:
        return None
    return r.f[FOOTPRINT_INDEX]


def children(pid):
    buf = (ctypes.c_int * 64)()
    n = libc.proc_listchildpids(pid, buf, ctypes.sizeof(buf))
    return [buf[i] for i in range(max(0, n))]


def name(pid):
    buf = ctypes.create_string_buffer(64)
    libc.proc_name(pid, buf, 64)
    return buf.value.decode(errors="replace")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("pid", type=int)
    ap.add_argument("--secs", type=float, default=12.0)
    ap.add_argument("--every", type=float, default=0.02)
    ap.add_argument("--label", default="")
    ap.add_argument("--steps", action="store_true", help="also print each change of 5 MiB or more")
    args = ap.parse_args()
    t0 = time.time()
    names, series = {args.pid: name(args.pid) or "root"}, {}
    while time.time() - t0 < args.secs:
        for pid in children(args.pid):
            names.setdefault(pid, name(pid))
        now = time.time() - t0
        for pid, who in names.items():
            fp = footprint(pid)
            if fp is not None:
                series.setdefault((who, pid), []).append((now, fp))
        time.sleep(args.every)
    for (who, pid), rows in sorted(series.items()):
        peak_t, peak = max(rows, key=lambda r: r[1])
        above = [t for t, fp in rows if fp > peak / 2]
        mib = lambda b: b / 1048576
        if args.steps:
            shown = rows[0][1]
            steps = [f"{mib(shown):.0f} at {rows[0][0]:.1f}s"]
            for t, fp in rows:
                if abs(fp - shown) >= 5 * 1048576:
                    shown = fp
                    steps.append(f"{mib(fp):.0f} at {t:.1f}s")
            print(f"footprint {args.label} {who}: MiB " + ", ".join(steps))
        print(f"footprint {args.label} {who} pid {pid}: peak {mib(peak):.1f} MiB at {peak_t:.2f}s, "
              f"above half of it from {above[0]:.2f}s to {above[-1]:.2f}s, "
              f"last {mib(rows[-1][1]):.1f} MiB at {rows[-1][0]:.1f}s, {len(rows)} readings")


if __name__ == "__main__":
    main()
