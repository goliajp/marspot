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
# from loginwindow's line naming a launched app to the Dock's insert (up to
# 59 ms on mini2, 2026-11-27): it tells the probe's inserts from the
# suite's, which all count; an insert named for no app counts as the suite's
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
    """inserts and removal lines by reason, removal events (a removal line right after a
    removal line of another reason within a second is the same removal logged twice), the
    lowest count of inserts less removal events at any point (below zero, a tile the Dock
    held before the lines began went), and the removal lines taken as such a repeat"""
    ins, rem, events, prev, low, twice = collections.Counter(), collections.Counter(), 0, None, 0, []
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
            twice.append(l)
            continue
        events += 1
        prev = (t, reason)
        low = min(low, sum(ins.values()) - events)
    return ins, rem, events, low, twice


def strip_url(p):
    p = re.sub(r"^file://", "", p.strip()).rstrip("/")
    return p[len("/private"):] if p.startswith("/private/") else p


def attributed(lines):
    """(line number from 1, insert index, Dock pid, app) for each insert line, the app or None.
    The apps named for an insert are those loginwindow names within ATTRIBUTE_S of it, before
    or after: the name comes first nearly always (1 of 608 inserts on mini2, 2026-11-27
    21:30-23:07, had it 5 ms after), and a program is named again around its insert (the
    probes are named three times per start), so the nearest name says nothing.
      - another insert within ATTRIBUTE_S: None, two tiles and the names cannot be paired
      - one app named: that app
      - several named: the one of them known to take a tile, or None when that is not one app.
        Known to take a tile: the Dock probe (a regular app built for it), and an app named
        alone next to an insert alone elsewhere in the lines.  A program named here that is
        known to take no tile (another project's headless browser, named 1 ms nearer the
        probe's insert than the probe on mini2, 2026-11-28 01:45:32) is not a candidate, since
        the Dock inserted one tile in the window.  A program whose first tile ever is this
        one is not known yet: then the probe can be named for it, the residue of the rule"""
    urls = [(stamp(l), strip_url(l.split(URL, 1)[1])) for l in lines if URL in l]
    ins = []
    for n, l in enumerate(lines, 1):
        m = re.search(r"Inserting tile: .* at index: (\d+)", l)
        if m:
            ins.append((n, int(m.group(1)), dock_pid(l), stamp(l)))

    def near(t):
        return abs(t.total_seconds()) <= ATTRIBUTE_S

    def named(n, t):
        crowded = any(near(t - u) for k, _, _, u in ins if k != n)
        return crowded, {u for ut, u in urls if near(t - ut)}

    seen = [(n, i, p) + named(n, t) for n, i, p, t in ins]
    takers = {next(iter(u)) for _, _, _, c, u in seen if not c and len(u) == 1}
    out = []
    for n, i, p, crowded, apps in seen:
        known = [u for u in apps if u in takers or u.endswith(PROBE)]
        app = None
        if not crowded and len(apps) == 1:
            app = next(iter(apps))
        elif not crowded and len(known) == 1:
            app = known[0]
        out.append((n, i, p, app))
    return out


def inserts(text):
    """(insert index, the app loginwindow named next to it or None) for each insert"""
    return [(i, u) for _, i, _, u in attributed((text or "").splitlines())]


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


def at_once(text, base):
    """the most tiles of the suite the Dock held at once, from the inserts after the probe's.
    Every insert but the probe's counts, whatever program it is for: the limit is the
    number the suite declares for all the programs it opens on purpose.  The count starts
    one above the probe's index before the suite: the Dock logs a removal line once the
    probe is gone, but a removal line does not say which tile went, so the probe's own tile
    may still hold its index when the suite's first tile comes.
    An insert at or below the probe's index lowers the start to it (a tile went: stricter,
    never laxer).

    Counted from one above, the count is one low when the probe's tile goes after the
    suite's first tile came: A goes in at base+1 beside the probe's tile, the probe's tile
    goes and A moves down to base, B goes in at base+1 and C at base+2; three tiles of the
    suite are held and base+2 - (base+1) + 1 = 2 is counted.  The probe before the suite
    waits for the Dock's removal line after it and one second more, so that order does not
    come in a run; when the log shows it anyway (probe_went_late) the count is not read"""
    ins = [(i, u) for i, u in inserts(text) if not (u and u.endswith(PROBE))]
    if not ins:
        return 0, ""
    idx = [i for i, _ in ins]
    low = min([base + 1] + idx)
    span = "insert index %d..%d, %d before the suite" % (min(idx), max(idx), base)
    return max(idx) - low + 1, span


def probe_went_late(text):
    """the time of the suite's first insert logged after the probe's first insert in the window
    (the probe before the suite) and before the Dock's first removal line after it, or None"""
    lines = (text or "").splitlines()
    att = attributed(lines)
    first = next((n for n, _, _, u in att if u and u.endswith(PROBE)), None)
    if first is None:
        return None
    gone = next((n for n, l in enumerate(lines, 1) if n > first and REMOVE in l), len(lines) + 1)
    early = [n for n, _, _, u in att if first < n < gone and not (u and u.endswith(PROBE))]
    return lines[early[0] - 1][:23] if early else None


def log_check(d, b, read, most, kind, left, unread):
    """most: the tiles of the suite the Dock may hold at once, as the suite declares"""
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
        ins, rem, events, _, _ = dock_count(lines)
        print("dock: inserts=%d removal_lines=%d removal_events=%d" % (sum(ins.values()), sum(rem.values()), events))
        print("dock: inserts by reason: " + by_reason(ins))
        print("dock: removals by reason: " + by_reason(rem))
        if pid is None:
            unread.append("dock: tiles at once: the probe gave no index before the suite")
            return
        if others:
            unread.append("dock: tiles at once: a Dock other than %s logged tiles in the suite (%s)" % (pid, " ".join(others)))
            return
        peak, span = at_once(dock, int(b["probe_index"]))
        late = probe_went_late(dock)
    except ValueError:
        unread.append("dock log: a tile line without a time")
        return
    print("dock: tiles at once=%d (most allowed %d%s)" % (peak, most, ", " + span if span else ""))
    if peak > most:
        line = "dock: %d tiles at once (%s), more than %d" % (peak, span, most)
        # with another job on the host its tiles count too: whose cannot be told, so it is
        # neither passed nor blamed
        (left if kind == "alone" else unread).append(line + ("" if kind == "alone" else ", with another job on the host"))
    elif late:
        unread.append("dock: tiles at once: the suite's tile at %s came before the Dock logged the probe's tile gone,"
                      " so the count can be one low" % late)


def balanced(text):
    """(why the Dock's lines in the suite do not account for every tile or None, the removal
    lines with no insert of their own): each insert named for an app, as many removals as
    inserts, and no removal before an insert it can be for.  Next to another job an index
    equal before and after can be a tile the suite left behind one the other job's app, gone
    just before the snap, had still held: its removal then has no insert in the window.
    Removal lines name no tile, so one that comes after a tile of the window that stays is not
    told from that tile's own; nor is a removal line taken as the one before it logged twice
    from a second removal (those are the lines returned)"""
    if text is None or not text.startswith("Timestamp"):
        return "log show gave no output", []
    try:
        unnamed = sum(1 for _, _, _, u in attributed(text.splitlines()) if u is None)
        ins, _, events, low, twice = dock_count(tile_lines(text))
    except ValueError:
        return "a tile line without a time", []
    if unnamed:
        return "%d insert(s) named for no app" % unnamed, twice
    if low < 0:
        return "a removal with no insert of the suite's window before it", twice
    if sum(ins.values()) != events:
        return "%d insert(s) against %d removal(s)" % (sum(ins.values()), events), twice
    return None, twice


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
    elif e == s and kind != "alone":
        why, twice = balanced(read(d, "dock.log"))
        if why:
            unread.append("dock probe: index %d before and after the suite next to another job, and the Dock's"
                          " lines in the suite do not account for every tile (%s)" % (s, why))
        else:
            # passed; what is left that the lines cannot tell is shown
            print("info: dock: removal lines with no matching insert (taken as the one before logged twice): "
                  + ("; ".join("%s %s" % (l[:23], l.rsplit("for reason: ", 1)[-1].strip()) for l in twice) or "none"))
    elif e < s:
        unread.append("dock: the probe's index fell from %d to %d: a tile went during the suite, and one the suite"
                      " left could be hidden behind it" % (s, e))
