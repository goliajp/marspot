# bin/_dock-probe.py — reads a `log stream --style compact` of the Dock's
# tile lines and loginwindow's "Got App URL" lines as it is written, for
# bin/_dock-probe.sh:
#
#   ready SECS FILE...               the stream is attached: its banner is
#                                    in one of FILEs (its output, its errors)
#   insert FILE EXE DOCK_PID SECS    the Dock inserted EXE's tile: prints
#                                    `index=N` and `line=K` (its line number)
#   removed FILE FROM DOCK_PID SECS  the Dock logged a tile removal after
#                                    line FROM
#
# Exit 0 when seen, 1 when not within SECS, 3 when a tile line comes from
# a Dock other than DOCK_PID (it restarted: its indexes are not comparable),
# 4 at once when an insert or app line has no time it can be read by (printed
# as `untimed=LINE`).
# Which app an insert is for is decided as in the suite's check
# (_e2e_dock.attributed).
import sys, time

sys.dont_write_bytecode = True
import _e2e_dock as dock  # noqa: E402

TILES = "[com.apple.dock:dock-tiles]"
BANNER = "Filtering the log data"


def find_insert(lines, exe, pid):
    """(index, line number) of the first tile the Dock pid inserted that is named for exe;
    "other" when another Dock logged a tile line before it, or None"""
    named = {n: (i, u) for n, i, _, u in dock.attributed(lines)}
    for n, l in enumerate(lines, 1):
        if TILES not in l:
            continue
        if dock.dock_pid(l) != pid:
            return "other"
        if n in named and named[n][1] == dock.strip_url(exe):
            return named[n][0], n
    return None


def find_removal(lines, start, pid):
    for l in lines[start:]:
        if TILES not in l:
            continue
        if dock.dock_pid(l) != pid:
            return "other"
        if "Removing tiles" in l:
            return True
    return None


def read_lines(path):
    with open(path, errors="replace") as f:
        # a line still being written is left for the next look
        return f.read().split("\n")[:-1]


class Untimed(Exception):
    pass


def untimed(lines):
    for l in lines:
        if dock.INSERT in l or dock.URL in l:
            try:
                dock.stamp(l)
            except ValueError:
                return l
    return None


def poll(paths, secs, look):
    """look's answer once it has one, None at SECS; Untimed(line) at once when a line it reads
    has no time: waiting does not give it one"""
    end = time.monotonic() + secs
    while True:
        lines = [l for p in paths for l in read_lines(p)]
        try:
            got = look(lines)
        except ValueError:
            raise Untimed(untimed(lines))
        if got is not None or time.monotonic() >= end:
            return got
        time.sleep(0.1)


def main(a):
    try:
        return answer(a)
    except Untimed as e:
        print("untimed=%s" % e.args[0])
        return 4


def answer(a):
    if a[0] == "ready":
        got = poll(a[2:], float(a[1]), lambda ls: True if any(BANNER in l for l in ls) else None)
        return 0 if got else 1
    if a[0] == "insert":
        got = poll([a[1]], float(a[4]), lambda ls: find_insert(ls, a[2], a[3]))
        if got == "other":
            return 3
        if got is None:
            return 1
        print("index=%d\nline=%d" % got)
        return 0
    if a[0] == "removed":
        got = poll([a[1]], float(a[4]), lambda ls: find_removal(ls, int(a[2]), a[3]))
        return 3 if got == "other" else 0 if got else 1
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
