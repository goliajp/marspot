# the Dock's part of bin/_e2e-suite-clean.py: whether the suite left a
# tile, from the Dock probe's insert index before and after it
# (bin/_dock-probe.sh), and how many tiles of the suite the Dock held at
# once, from its own log.
#
# Counting the log's insert and removal lines says nothing about what is
# left: one "Removing tiles identical to" line may remove no tile, and a
# tile of an app that went from regular to accessory and back within the
# Dock's lazy delete can stay after its process is gone, a tile no app
# list or `defaults` key shows.  An insert index is the Dock's own count
# of the tiles before the new one; the probe's index before the suite is
# where a tile of the suite goes when the Dock holds none of its others,
# so an insert at that index plus k says the Dock held k more at once.
# Indexes are compared only under one Dock process: a restarted Dock
# counts afresh.
import collections, re
from datetime import datetime

INSERT, REMOVE, URL = "Inserting tile", "Removing tiles identical to", "Got App URL: "
# tiles of ours at once: the app, and its next image's tile while the
# Dock still holds the last one
MOST_AT_ONCE = 2
# from loginwindow's line naming a launched app to the Dock's insert (up to
# 59 ms on mini2, 2026-11-27): only used to allow a tile for each program
# of the run that is not the app, never to excuse an insert
ATTRIBUTE_S = 0.2
PROBE = "/marspot-dockprobe"


def tile_lines(text):
    return [l for l in (text or "").splitlines() if INSERT in l or REMOVE in l]


def stamp(line):
    return datetime.strptime(line[:23], "%Y-%m-%d %H:%M:%S.%f")


def dock_pid(line):
    m = re.search(r" Dock\[(\d+):", line)
    return m.group(1) if m else None


def dock_count(lines):
    """inserts and removal lines by reason, and removal events: a removal line right after
    a removal line of another reason within a second is the same removal logged twice"""
    ins, rem, events, prev = collections.Counter(), collections.Counter(), 0, None
    for l in lines:
        m = re.search(r"for reason: (.*?)\s*$", l)
        reason = m.group(1) if m else "?"
        t = stamp(l)
        if INSERT in l:
            ins[reason] += 1
            prev = None
            continue
        rem[reason] += 1
        if prev and prev[1] != reason and (t - prev[0]).total_seconds() <= 1.0:
            prev = None
            continue
        events += 1
        prev = (t, reason)
    return ins, rem, events


def inserts(text):
    """(insert index, the app loginwindow named just before it or None) for each insert"""
    out, last = [], None
    for l in (text or "").splitlines():
        if URL in l:
            u = l.split(URL, 1)[1].strip()
            u = re.sub(r"^file://(/private)?", "", u).rstrip("/")
            last = (stamp(l), u)
        elif INSERT in l:
            m = re.search(r"at index: (\d+)", l)
            if not m:
                continue
            near = last and (stamp(l) - last[0]).total_seconds() <= ATTRIBUTE_S
            out.append((int(m.group(1)), last[1] if near else None))
    return out


def by_reason(c):
    return "; ".join("%d %s" % (n, r) for r, n in c.most_common()) or "none"


def probe(text):
    """the probe's answer as a dict, or None when it did not run"""
    if text is None:
        return None
    return dict(l.split("=", 1) for l in text.splitlines() if "=" in l)


def probe_snap(d, read):
    p = probe(read(d, "probe-start.txt"))
    if p is None:
        return ["probe_unread=the Dock probe did not run"]
    if "unread" in p or "index" not in p:
        return ["probe_unread=" + p.get("unread", "the Dock probe gave no index")]
    return ["probe_dock_pid=" + p["dock_pid"], "probe_index=" + p["index"]]


def at_once(text, base, ours, under):
    """the most tiles of the suite the Dock held at once and the limit, from the inserts after
    the probe's: base is the probe's index before the suite, or the lowest insert when a tile
    went during the suite (stricter).  A program of this run that is not the app (a helper
    registering as a regular app) is the suite's subject and adds one to the limit"""
    ins = [(i, u) for i, u in inserts(text) if not (u and u.endswith(PROBE))]
    if not ins:
        return 0, MOST_AT_ONCE, ""
    idx = [i for i, _ in ins]
    low = min([base] + idx)
    helpers = {u for _, u in ins if u and (ours(u) or under(u)) and "marspot" not in u.lower()}
    span = "insert index %d..%d, %d before the suite" % (min(idx), max(idx), base)
    return max(idx) - low + 1, MOST_AT_ONCE + len(helpers), span


def log_check(d, b, read, ours, under, kind, left, unread):
    dock = read(d, "dock.log")
    if dock is None or not dock.startswith("Timestamp"):
        unread.append("dock log: log show gave no output")
        return
    lines = tile_lines(dock)
    if not lines and b.get("dock_live") != "1":
        unread.append("dock log: no tile line in the suite nor in the 24 h probe (predicate or format dead)")
        return
    pid = b.get("probe_dock_pid")
    others = sorted({dock_pid(l) for l in lines} - {pid})
    try:
        ins, rem, events = dock_count(lines)
        print("dock: inserts=%d removal_lines=%d removal_events=%d" % (sum(ins.values()), sum(rem.values()), events))
        print("dock: inserts by reason: " + by_reason(ins))
        print("dock: removals by reason: " + by_reason(rem))
        if pid is None:
            unread.append("dock: tiles at once: the probe gave no index before the suite")
            return
        if others:
            unread.append("dock: tiles at once: a Dock other than %s logged tiles in the suite (%s)" % (pid, " ".join(others)))
            return
        peak, most, span = at_once(dock, int(b["probe_index"]), ours, under)
    except ValueError:
        unread.append("dock log: a tile line without a time")
        return
    print("dock: tiles at once=%d (most allowed %d%s)" % (peak, most, ", " + span if span else ""))
    if peak > most:
        line = "dock: %d tiles at once (%s), more than %d" % (peak, span, most)
        # with another job on the host its tiles count too: whose cannot be told, so it is
        # neither passed nor blamed
        (left if kind == "alone" else unread).append(line + ("" if kind == "alone" else ", with another job on the host"))


def probe_check(d, b, read, kind, fg_same, left, unread):
    """the probe's index after the suite against before it, under one Dock"""
    if "probe_unread" in b or "probe_index" not in b:
        unread.append("dock probe before the suite: " + b.get("probe_unread", "not run"))
        return
    end = probe(read(d, "probe-end.txt"))
    if end is None:
        unread.append("dock probe after the suite: did not run")
        return
    if "unread" in end or "index" not in end:
        unread.append("dock probe after the suite: " + end.get("unread", "gave no index"))
        return
    if end.get("dock_pid") != b["probe_dock_pid"]:
        unread.append("dock probe: the Dock restarted during the suite (pid %s, then %s): its tiles cannot be compared"
                      % (b["probe_dock_pid"], end.get("dock_pid")))
        return
    s, e = int(b["probe_index"]), int(end["index"])
    first = end.get("first_index")
    print("dock probe: index before=%d after=%d%s" % (s, e, " (%s a moment earlier)" % first if first else ""))
    if e > s:
        line = "dock: %d tile(s) more after the suite than before (the probe's index %d, then %d)" % (e - s, s, e)
        if kind == "alone" or (kind == "shared" and fg_same):
            left.append(line)
        else:
            unread.append(line + "; another job was on the host and the apps running changed, so whose cannot be told")
    elif e < s:
        unread.append("dock: the probe's index fell from %d to %d: a tile went during the suite, and one the suite"
                      " left could be hidden behind it" % (s, e))
