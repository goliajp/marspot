# the Dock's part of bin/_e2e-suite-clean.py: its tiles before and after
# the suite as the Dock lists them (bin/_dock-tiles.py), and how many it
# held at once during the suite, from its own log.
#
# Counting the log's insert and removal lines says nothing about what is
# left: one "Removing tiles identical to" line may remove no tile, and a
# tile of an app that went from regular to accessory and back within the
# Dock's lazy delete can stay after its process is gone, a tile no app
# list or `defaults` key shows.  Its insert index is the Dock's own count
# of the tiles before it, so the span of the indexes in the suite is how
# many tiles the suite had at once.
import collections, json, re
from datetime import datetime

INSERT, REMOVE, URL = "Inserting tile", "Removing tiles identical to", "Got App URL: "
# tiles of ours at once: the app, and its next image's tile while the
# Dock still holds the last one
MOST_AT_ONCE = 2
# from loginwindow's line naming a launched app to the Dock's insert
ATTRIBUTE_S = 0.2


def tile_lines(text):
    return [l for l in (text or "").splitlines() if INSERT in l or REMOVE in l]


def stamp(line):
    return datetime.strptime(line[:23], "%Y-%m-%d %H:%M:%S.%f")


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


def at_once(text, ours, under):
    """the most tiles the suite had at once and the limit, and whether every insert was ours.
    A program of this run that is not the app (a probe registering as a regular app) is
    the suite's subject and adds one to the limit; an insert nobody is named for is
    counted, never excused"""
    ins = inserts(text)
    if not ins:
        return 0, MOST_AT_ONCE, True, ""
    idx = [i for i, _ in ins]
    mine = lambda u: bool(u) and (ours(u) or under(u))
    helpers = {u for _, u in ins if mine(u) and "marspot" not in u.lower()}
    all_ours = all(mine(u) for _, u in ins)
    span = "insert index %d..%d" % (min(idx), max(idx))
    return max(idx) - min(idx) + 1, MOST_AT_ONCE + len(helpers), all_ours, span


def log_check(d, b, read, ours, under, left, wide, wide_unread):
    dock = read(d, "dock.log")
    if dock is None or not dock.startswith("Timestamp"):
        wide_unread.append("dock log: log show gave no output")
        return
    lines = tile_lines(dock)
    if not lines and b.get("dock_live") != "1":
        wide_unread.append("dock log: no tile line in the suite nor in the 24 h probe (predicate or format dead)")
        return
    try:
        ins, rem, events = dock_count(lines)
        peak, most, all_ours, span = at_once(dock, ours, under)
    except ValueError:
        wide_unread.append("dock log: a tile line without a time")
        return
    print("dock: inserts=%d removal_lines=%d removal_events=%d" % (sum(ins.values()), sum(rem.values()), events))
    print("dock: inserts by reason: " + by_reason(ins))
    print("dock: removals by reason: " + by_reason(rem))
    print("dock: tiles at once=%d (most allowed %d%s)" % (peak, most, ", " + span if span else ""))
    if peak > most:
        line = "dock: %d tiles at once (%s), more than %d" % (peak, span, most)
        (left if all_ours else wide).append(line)


def tiles(text):
    """the Dock's tiles as [subrole, title], or None when it was not read"""
    if text is None or text.strip() == "untrusted":
        return None
    return [l.split("\t")[1:4:2] for l in text.splitlines() if l.startswith("tile\t") and l.count("\t") == 3]


def snap(d, read):
    t = tiles(read(d, "dock-tiles.txt"))
    return ["dock_tiles=" + json.dumps(t)] if t is not None else []


def unread_why(text):
    if text is None:
        return "dock tiles: the Dock's own list could not be read"
    return "dock tiles: this login is not trusted for accessibility, so the Dock's own list was not read"


def tiles_check(d, b, read, ours, left, wide, unread):
    """every tile the Dock lists after the suite that it did not list before"""
    text = read(d, "dock-tiles.txt")
    now = tiles(text)
    if "dock_tiles" not in b:
        unread.append("dock tiles: not read before the suite")
        return
    if now is None:
        unread.append(unread_why(text))
        return
    was = collections.Counter(tuple(t) for t in json.loads(b["dock_tiles"]))
    cur = collections.Counter(tuple(t) for t in now)
    print("dock tiles: before=%d after=%d" % (sum(was.values()), sum(cur.values())))
    for (sub, title), n in (cur - was).items():
        line = "dock tile added: %s %s%s" % (title or "(no title)", sub, " x%d" % n if n > 1 else "")
        (left if ours(title) else wide).append(line)
    for (sub, title), n in (was - cur).items():
        print("dock: tile gone: %s %s" % (title or "(no title)", sub))
