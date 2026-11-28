#!/usr/bin/env python3
"""_webpane_check.py: what bin/test-webpane.sh reads.  Standard library only.

    diff SURFACE.png SHOT.png X Y W H OUT.png [OX OY]
        Compares the W by H pixels at X,Y of SURFACE (the window's buffer)
        with the W by H at OX,OY (default the top left) of SHOT (the page
        drawn by the engine alone), RGB only; writes the region of SURFACE
        to OUT.  Prints `differ=N total=N max_delta=N colors=N`.

    wait-drawn LOG SESSION AFTER SECS [SCROLL_X SCROLL_Y]
        Waits up to SECS for a WEB_PANE_DRAWN line of web pane SESSION
        past line AFTER of LOG (scrolled to SCROLL_X,SCROLL_Y when given);
        prints its line number and the line.  Exits 1 when none came.

    scroll-stats LOG OUT.json
        The latencies the core logged for scrolls drawn (web_pane.scroll):
        prints and writes their count, median, p99 and largest, in us.

    usage NAME=PID...
        Prints `NAME cpu_ns wakeups` per process from proc_pid_rusage, one
        line each; exits 2 when a process cannot be read.

    thread PID PREFIX
        Prints `threads=N cpu_ns=N`, the CPU time of PID's threads whose
        name starts with PREFIX; exits 2 when there is none.
"""
import ctypes
import struct
import sys
import zlib


def read_png(path, rows=None):
    data = open(path, "rb").read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        sys.exit(f"{path}: not a PNG")
    at, idat, head = 8, b"", None
    while at < len(data):
        n, kind = struct.unpack(">I4s", data[at:at + 8])
        body = data[at + 8:at + 8 + n]
        if kind == b"IHDR":
            head = struct.unpack(">IIBBBBB", body)
        elif kind == b"IDAT":
            idat += body
        at += 12 + n
    w, h, depth, ctype, _, _, interlace = head
    if depth != 8 or ctype not in (2, 6) or interlace:
        sys.exit(f"{path}: depth {depth} colour type {ctype} interlace {interlace} is not read here")
    bpp = 3 if ctype == 2 else 4
    raw = zlib.decompress(idat)
    stride = w * bpp
    out = bytearray(h * stride)
    prev = bytearray(stride)
    for y in range(h if rows is None else min(h, rows)):
        f = raw[y * (stride + 1)]
        line = bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        for i in range(stride if f else 0):
            a = line[i - bpp] if i >= bpp else 0
            b = prev[i]
            c = prev[i - bpp] if i >= bpp else 0
            if f == 1:
                line[i] = (line[i] + a) & 255
            elif f == 2:
                line[i] = (line[i] + b) & 255
            elif f == 3:
                line[i] = (line[i] + (a + b) // 2) & 255
            elif f == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pr = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 255
        out[y * stride:(y + 1) * stride] = line
        prev = line
    return w, h, bpp, out


def write_png(path, w, h, rgb):
    rows = b"".join(b"\0" + bytes(rgb[y * w * 3:(y + 1) * w * 3]) for y in range(h))
    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))
    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(rows, 6)) + chunk(b"IEND", b"")
    open(path, "wb").write(png)


def diff(surface, shot, x, y, w, h, out, ox=0, oy=0):
    sw, sh, sb, s = read_png(surface)
    pw, ph, pb, p = read_png(shot, oy + h)
    if x + w > sw or y + h > sh:
        sys.exit(f"the region {w}x{h}+{x}+{y} is not inside the surface {sw}x{sh}")
    w, h = min(w, pw - ox), min(h, ph - oy)
    differ, worst, colors, crop = 0, 0, set(), bytearray()
    for r in range(h):
        for c in range(w):
            i = ((y + r) * sw + x + c) * sb
            j = ((oy + r) * pw + ox + c) * pb
            a, b = s[i:i + 3], p[j:j + 3]
            crop += a
            colors.add(bytes(a))
            d = max(abs(a[k] - b[k]) for k in range(3))
            if d:
                differ += 1
                worst = max(worst, d)
    write_png(out, w, h, crop)
    print(f"differ={differ} total={w * h} max_delta={worst} colors={len(colors)}")


class RusageV2(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [(n, ctypes.c_uint64) for n in (
        "user_time", "system_time", "pkg_idle_wkups", "interrupt_wkups", "pageins", "wired_size",
        "resident_size", "phys_footprint", "proc_start_abstime", "proc_exit_abstime",
        "child_user_time", "child_system_time", "child_pkg_idle_wkups", "child_interrupt_wkups",
        "child_pageins", "child_elapsed_abstime", "diskio_bytesread", "diskio_byteswritten")]


def usage(pairs):
    libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
    tb = (ctypes.c_uint32 * 2)()
    libc.mach_timebase_info(tb)
    for pair in pairs:
        name, pid = pair.split("=")
        r = RusageV2()
        if libc.proc_pid_rusage(int(pid), 2, ctypes.byref(r)) != 0:
            print(f"{name}: pid {pid} cannot be read (errno {ctypes.get_errno()})", file=sys.stderr)
            sys.exit(2)
        ns = (r.user_time + r.system_time) * tb[0] // tb[1]
        print(f"{name} {ns} {r.interrupt_wkups}")


class ThreadInfo(ctypes.Structure):
    _fields_ = [(n, ctypes.c_uint64) for n in ("user_time", "system_time")] + [
        (n, ctypes.c_int32) for n in (
            "cpu_usage", "policy", "run_state", "flags", "sleep_time", "curpri", "priority", "maxpriority")
    ] + [("name", ctypes.c_char * 64)]


def thread(pid, prefix):
    libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
    ids = (ctypes.c_uint64 * 512)()
    n = libc.proc_pidinfo(pid, 6, ctypes.c_uint64(0), ids, ctypes.sizeof(ids))
    total, count = 0, 0
    for h in ids[: max(n, 0) // 8]:
        ti = ThreadInfo()
        got = libc.proc_pidinfo(pid, 5, ctypes.c_uint64(h), ctypes.byref(ti), ctypes.sizeof(ti))
        if got == ctypes.sizeof(ti) and ti.name.decode(errors="replace").startswith(prefix):
            total += ti.user_time + ti.system_time
            count += 1
    if count == 0:
        print(f"no thread of pid {pid} is named {prefix}*", file=sys.stderr)
        sys.exit(2)
    print(f"threads={count} cpu_ns={total}")


def wait_drawn(log, session, after, secs, scroll=None):
    import time
    deadline = time.time() + secs
    while time.time() < deadline:
        try:
            lines = open(log, errors="replace").read().splitlines()
        except FileNotFoundError:
            lines = []
        for n, line in enumerate(lines[after:], after + 1):
            if "\tWEB_PANE_DRAWN\t" not in line or f"session={session}" not in line.split():
                continue
            if scroll and not all(f"scroll_{k}={v}" in line.split() for k, v in zip("xy", scroll)):
                continue
            print(n, line)
            return
        time.sleep(0.2)
    sys.exit(1)


def scroll_stats(log, out):
    import json
    import re
    us = sorted(int(m.group(1)) for line in open(log, errors="replace")
                if "web_pane.scroll" in line for m in [re.search(r"latency_us=(\d+)", line)] if m)
    if not us:
        sys.exit("no scroll was logged as drawn")
    pick = lambda q: us[min(len(us) - 1, int(q * len(us)))]
    r = {"count": len(us), "p50_us": pick(0.5), "p99_us": pick(0.99), "max_us": us[-1]}
    open(out, "w").write(json.dumps(r) + "\n")
    print(" ".join(f"{k}={v}" for k, v in r.items()))


if __name__ == "__main__":
    if sys.argv[1:2] == ["wait-drawn"] and len(sys.argv) in (6, 8):
        wait_drawn(sys.argv[2], sys.argv[3], int(sys.argv[4]), float(sys.argv[5]), sys.argv[6:8] or None)
        sys.exit(0)
    if sys.argv[1:2] == ["scroll-stats"] and len(sys.argv) == 4:
        scroll_stats(sys.argv[2], sys.argv[3])
        sys.exit(0)
    if sys.argv[1:2] == ["diff"] and len(sys.argv) == 11:
        diff(sys.argv[2], sys.argv[3], *map(int, sys.argv[4:8]), sys.argv[8], *map(int, sys.argv[9:11]))
        sys.exit(0)
    if sys.argv[1:2] == ["thread"] and len(sys.argv) == 4:
        thread(int(sys.argv[2]), sys.argv[3])
        sys.exit(0)
    if sys.argv[1:2] == ["diff"] and len(sys.argv) == 9:
        diff(sys.argv[2], sys.argv[3], *map(int, sys.argv[4:8]), sys.argv[8])
    elif sys.argv[1:2] == ["usage"]:
        usage(sys.argv[2:])
    else:
        sys.exit(__doc__)
