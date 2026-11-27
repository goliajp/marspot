# bin/_dock-probe.py — reads a `log stream --style compact` of the Dock's
# tile lines and loginwindow's "Got App URL" lines as it is written, for
# bin/_dock-probe.sh:
#
#   ready FILE SECS                  the stream is attached
#   insert FILE EXE DOCK_PID SECS    the Dock inserted EXE's tile: prints
#                                    `index=N` and `line=K` (its line number)
#   removed FILE FROM DOCK_PID SECS  the Dock logged a tile removal after
#                                    line FROM
#
# Exit 0 when seen, 1 when not within SECS, 3 when a tile line comes from
# a Dock other than DOCK_PID (it restarted: its indexes are not comparable).
import re, sys, time
from datetime import datetime

sys.dont_write_bytecode = True

URL = "Got App URL: "
# loginwindow names the app a few milliseconds before the Dock inserts its
# tile (up to 59 ms on mini2, 2026-11-27); a tile inserted further than this
# from the name is not taken for the probe's
ATTRIBUTE_S = 0.2


def stamp(line):
    try:
        return datetime.strptime(line[:23], "%Y-%m-%d %H:%M:%S.%f")
    except ValueError:
        return None


def strip(p):
    p = re.sub(r"^file://", "", p.strip()).rstrip("/")
    return p[len("/private"):] if p.startswith("/private/") else p


def dock_pid(line):
    m = re.search(r" Dock\[(\d+):", line)
    return m.group(1) if m else None


def find_insert(lines, exe, pid):
    """(index, line number) of the first tile the Dock pid inserted whose nearest naming line
    of loginwindow, within ATTRIBUTE_S either side, names exe; "other" when another Dock
    logged a tile line, or None.  The name comes first nearly always; 1 of 608 inserts on
    mini2 (2026-11-27 21:30-23:07) had it 5 ms after"""
    urls = [(stamp(l), strip(l.split(URL, 1)[1])) for l in lines if URL in l]
    for n, l in enumerate(lines, 1):
        if "[com.apple.dock:dock-tiles]" not in l:
            continue
        if dock_pid(l) != pid:
            return "other"
        m = re.search(r"Inserting tile: .* at index: (\d+)", l)
        t = stamp(l)
        if not m or not t:
            continue
        near = [(abs((t - ut).total_seconds()), u) for ut, u in urls if ut]
        near = [c for c in near if c[0] <= ATTRIBUTE_S]
        if near and min(near)[1] == strip(exe):
            return int(m.group(1)), n
    return None


def find_removal(lines, start, pid):
    for l in lines[start:]:
        if "[com.apple.dock:dock-tiles]" not in l:
            continue
        if dock_pid(l) != pid:
            return "other"
        if "Removing tiles" in l:
            return True
    return None


def poll(path, secs, look):
    end = time.monotonic() + secs
    while True:
        with open(path, errors="replace") as f:
            # a line still being written is left for the next look
            lines = f.read().split("\n")[:-1]
        got = look(lines)
        if got is not None or time.monotonic() >= end:
            return got
        time.sleep(0.1)


def main(a):
    if a[0] == "ready":
        got = poll(a[1], float(a[2]), lambda ls: True if any("Filtering the log data" in l for l in ls) else None)
        return 0 if got else 1
    if a[0] == "insert":
        got = poll(a[1], float(a[4]), lambda ls: find_insert(ls, a[2], a[3]))
        if got == "other":
            return 3
        if got is None:
            return 1
        print("index=%d\nline=%d" % got)
        return 0
    if a[0] == "removed":
        got = poll(a[1], float(a[4]), lambda ls: find_removal(ls, int(a[2]), a[3]))
        return 3 if got == "other" else 0 if got else 1
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
