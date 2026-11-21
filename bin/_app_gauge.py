#!/usr/bin/env python3
"""Helpers for the sandbox-app gauges (bin/measure-app-footprint.sh,
bin/measure-startup.sh).

    _app_gauge.py exec STAMP PROG [ARGS...]
        write the wall clock in ms to STAMP, then exec PROG in place:
        same pid, nothing between the stamp and the exec
    _app_gauge.py wait-event LOG EVENT SECS
        print the ms field of the first EVENT line in the app log, once
        it is there; exit 1 after SECS without one
    _app_gauge.py idle-footprint PID [PID...]
        wait until the pids together use under IDLE_CORES of a core over
        a 2 s window (at most 60 s), then print one JSON object: each
        pid's phys_footprint in bytes, the sum, and how long it waited
    _app_gauge.py boot LOG
        ms from the shell's STARTUP line to the first of each boot event
"""
import ctypes
import json
import os
import sys
import time

sys.dont_write_bytecode = True

IDLE_CORES = 0.02
libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)


class Rusage(ctypes.Structure):
    # rusage_info_v4 after the uuid: user, system, ..., phys_footprint is the eighth
    _fields_ = [("uuid", ctypes.c_uint8 * 16), ("f", ctypes.c_uint64 * 35)]


class Timebase(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


def rusage(pid):
    r = Rusage()
    if libc.proc_pid_rusage(pid, 4, ctypes.byref(r)) != 0:
        raise SystemExit(f"proc_pid_rusage({pid}) failed: errno {ctypes.get_errno()}")
    return r


def cpu_ns(pids):
    tb = Timebase()
    libc.mach_timebase_info(ctypes.byref(tb))
    return sum((r.f[0] + r.f[1]) for r in map(rusage, pids)) * tb.numer / tb.denom


def idle_footprint(pids):
    t0 = time.time()
    while True:
        a = cpu_ns(pids)
        time.sleep(2)
        used = (cpu_ns(pids) - a) / 2e9
        if used < IDLE_CORES or time.time() - t0 > 60:
            break
    fp = {str(p): rusage(p).f[7] for p in pids}
    print(json.dumps({"footprint": fp, "sum": sum(fp.values()), "idle_cores": round(used, 4),
                      "idle": used < IDLE_CORES, "waited_s": round(time.time() - t0, 1)}))


def event_ms(log, event):
    try:
        for line in open(log, errors="replace"):
            c = line.split("\t")
            if len(c) > 7 and c[6] == event:
                return int(c[1])
    except OSError:
        pass
    return None


BOOT = ("STARTUP", "CORE_SPAWN", "CORE_BOOT", "SESSION_BOOT", "CORE_LOOP",
        "WINDOW_FIRST_FRAME", "WINDOW_FIRST_PRESENT", "WINDOW_FIRST_CONTENT")


def main():
    cmd, args = sys.argv[1], sys.argv[2:]
    if cmd == "exec":
        open(args[0], "w").write(f"{int(time.time() * 1000)}\n")
        os.execv(args[1], args[1:])
    elif cmd == "wait-event":
        log, event, secs = args[0], args[1], float(args[2])
        end = time.time() + secs
        while (ms := event_ms(log, event)) is None and time.time() < end:
            time.sleep(0.05)
        if ms is None:
            sys.exit(1)
        print(ms)
    elif cmd == "idle-footprint":
        idle_footprint([int(p) for p in args])
    elif cmd == "boot":
        t = {e: event_ms(args[0], e) for e in BOOT}
        s = t["STARTUP"]
        print(" ".join(f"{e}={v - s}" for e, v in t.items() if v is not None and s is not None))
    else:
        raise SystemExit(f"unknown command {cmd}")


if __name__ == "__main__":
    main()
