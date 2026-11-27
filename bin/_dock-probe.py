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
# as `untimed=LINE`), 5 when more than one insert has EXE named within
# ATTRIBUTE_S (another app's tile is among them; which is EXE's cannot be
# told), 6 when EXE's insert was seen but had not stood by SECS.
# Which app an insert is for is decided as in the suite's check
# (_e2e_dock.named_inserts), and an insert taken as EXE's stands only once
# the stream holds a line logged ATTRIBUTE_S past it or SETTLE_S went by
# with the same answer: a second insert or name logged just after it
# would make it unattributable, and a stream line can come late.  The gap
# left: an insert within ATTRIBUTE_S of EXE's that the stream delivers more
# than SETTLE_S late, after every line had gone quiet.
import sys, time

sys.dont_write_bytecode = True
import _e2e_dock as dock  # noqa: E402

TILES = "[com.apple.dock:dock-tiles]"
BANNER = "Filtering the log data"
SETTLE_S = 1.0


def find_insert(lines, exe, pid):
    """(index, line number, settled) of the one tile the Dock pid inserted next to exe's names
    and attributed to exe, settled when a later line is logged ATTRIBUTE_S past it; "many" when
    more than one insert has exe named within ATTRIBUTE_S (the probe starts once per stream);
    "other" when another Dock logged a tile line before the last of those; or None"""
    own = dock.strip_url(exe)
    near = [c for r in dock.probe_runs(lines, lambda u: u == own) for c in r]
    last = near[-1][0] if near else len(lines)
    if any(TILES in l and dock.dock_pid(l) != pid for l in lines[:last]):
        return "other"
    if len(near) > 1:
        return "many"
    if not near or not near[0][2]:
        return None
    n, i, _ = near[0]
    t = dock.stamp(lines[n - 1])
    later = any((dock.stamp(k) - t).total_seconds() > dock.ATTRIBUTE_S for k in lines[n:] if timed(k))
    return i, n, later


def timed(l):
    try:
        dock.stamp(l)
        return True
    except ValueError:
        return False


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


# an answer seen that had not stood by SECS
UNSETTLED = "unsettled"


def untimed(lines):
    for l in lines:
        if dock.INSERT in l or dock.URL in l:
            try:
                dock.stamp(l)
            except ValueError:
                return l
    return None


def poll(paths, secs, look, settle=None):
    """look's answer once it has one, None at SECS; Untimed(line) at once when a line it reads
    has no time: waiting does not give it one.  With settle, an answer stands once settle says
    so or it held SETTLE_S unchanged; one that has not stood by SECS is UNSETTLED"""
    end = time.monotonic() + secs
    held = None
    while True:
        lines = [l for p in paths for l in read_lines(p)]
        try:
            got = look(lines)
        except ValueError:
            raise Untimed(untimed(lines))
        now = time.monotonic()
        if settle is None:
            if got is not None or now >= end:
                return got
        elif got is None:
            held = None
        else:
            if held is None or held[0] != got:
                held = (got, now)
            if settle(got) or now - held[1] >= SETTLE_S:
                return got
        if now >= end:
            return None if held is None else UNSETTLED
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
        got = poll([a[1]], float(a[4]), lambda ls: find_insert(ls, a[2], a[3]),
                   lambda g: g in ("other", "many") or g[2])
        if got in ("other", "many", UNSETTLED):
            return {"other": 3, "many": 5, UNSETTLED: 6}[got]
        if got is None:
            return 1
        print("index=%d\nline=%d" % got[:2])
        return 0
    if a[0] == "removed":
        got = poll([a[1]], float(a[4]), lambda ls: find_removal(ls, int(a[2]), a[3]))
        return 3 if got == "other" else 0 if got else 1
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
