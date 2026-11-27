# bin/_e2e-suite-clean.py snap DIR LOCK_PID
#                         check DIR BEFORE LEFTOVERS_RC LEFTOVERS_OUT LOCK_PID PREFIX...
# the decision of bin/_e2e-suite-clean.sh over the inputs it saved in DIR.
# snap prints the before state as key=value lines; check prints the raw
# counts, what was found and last clean=0|1|2 (also the exit status).
# CLEAN_KNOWN holds PID:PATH records listed before the run, one per line.
import collections, json, os, re, sys
from datetime import datetime

DOCK_APP = "/System/Library/CoreServices/Dock.app"
INSERT, REMOVE = "Inserting tile", "Removing tiles identical to"


def read(d, n):
    p = os.path.join(d, n)
    return open(p, errors="replace").read() if os.path.exists(p) else None


def strip(p):
    return p[len("/private"):] if p.startswith("/private/") else p


def records(text):
    recs, cur = [], None
    for line in text.splitlines():
        m = re.match(r'\s*\d+\) "(.*)" ASN:', line)
        if m:
            cur = {"name": m.group(1), "type": "?", "pid": "?", "paths": []}
            recs.append(cur)
            continue
        if cur is None:
            continue
        for k in ("bundle path", "executable path"):
            p = re.search(k + r'="([^"]*)"', line)
            if p:
                cur["paths"].append(strip(p.group(1)))
        t = re.search(r'type="(\w+)"', line)
        if t:
            cur["type"] = t.group(1)
        p = re.search(r"pid = (-?\d+)", line)
        if p:
            cur["pid"] = p.group(1)
    return recs


def ours(s):
    return "marspot" in s.lower() or "/msp-" in s


def key(r):
    return "%s:%s" % (r["pid"], r["paths"][0] if r["paths"] else "")


def state(d):
    """the counts read now and the records, or None for the list when it is not the console login's"""
    c, why = {}, []
    ls = read(d, "lsappinfo.txt")
    recs = records(ls) if ls is not None else []
    if not any(p.startswith(DOCK_APP) for r in recs for p in r["paths"]):
        why.append("lsappinfo: no list of the console login (no Dock in it)")
        recs = None
    else:
        c["fg"] = sum(r["type"] == "Foreground" for r in recs)
    for k, n in (("persistent", "persistent.txt"), ("recent", "recent.txt")):
        t = read(d, n)
        if t is None:
            why.append("defaults: com.apple.dock %s could not be read" % k)
        else:
            c[k] = t.count('"file-label"')
    return c, recs, why


def tile_lines(text):
    return [l for l in (text or "").splitlines() if INSERT in l or REMOVE in l]


def tcc_requests(text):
    reqs = {}
    for line in (text or "").splitlines():
        try:
            msg = json.loads(line).get("eventMessage", "")
        except (ValueError, AttributeError):
            continue
        m = re.search(r"msgID=([^,]+)", msg)
        if not m:
            continue
        r = reqs.setdefault(m.group(1), {"svc": "?", "paths": []})
        s = re.search(r"service=(\w+)", msg)
        if s:
            r["svc"] = s.group(1)
        # responsible, accessing and requesting each name a binary_path
        for p in re.findall(r"(?:binary|responsible)_path=([^,}]+)", msg):
            if strip(p) not in r["paths"]:
                r["paths"].append(strip(p))
    return reqs


def holders(text, me):
    return [m.group(1) for m in re.finditer(r"(?m)^(\d+)\t(?:heavy|bench)\tholding", text) if m.group(1) != me]


def snap(d, me):
    c, recs, why = state(d)
    out = ["%s=%s" % kv for kv in c.items()] + ["unread=" + w for w in why]
    out += ["known=" + key(r) for r in recs or [] if any(ours(p) for p in r["paths"] + [r["name"]])]
    st = read(d, "status.txt")
    if st is not None:
        out.append("others_snap=" + " ".join(holders(st, me)))
    if read(d, "probe-dock.log") is not None or read(d, "probe-tcc.ndjson") is not None:
        out.append("dock_live=%d" % bool(tile_lines(read(d, "probe-dock.log"))))
        out.append("tcc_live=%d" % bool(tcc_requests(read(d, "probe-tcc.ndjson"))))
    print("\n".join(out))


def dock_count(lines):
    """inserts and removals by reason, and removal events: a removal line right after a
    removal line of another reason within a second is the same removal logged twice,
    which only ever undercounts removals"""
    ins, rem, events, prev = collections.Counter(), collections.Counter(), 0, None
    for l in lines:
        m = re.search(r"for reason: (.*?)\s*$", l)
        reason = m.group(1) if m else "?"
        t = datetime.strptime(l[:23], "%Y-%m-%d %H:%M:%S.%f")
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


def by_reason(c):
    return "; ".join("%d %s" % (n, r) for r, n in c.most_common()) or "none"


def dock_check(d, b, wide, wide_unread):
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
    except ValueError:
        wide_unread.append("dock log: a tile line without a time")
        return
    print("dock: inserts=%d removal_lines=%d removal_events=%d" % (sum(ins.values()), sum(rem.values()), events))
    print("dock: inserts by reason: " + by_reason(ins))
    print("dock: removals by reason: " + by_reason(rem))
    if sum(ins.values()) > events:
        wide.append("dock: %d tile insert(s), %d removal(s)" % (sum(ins.values()), events))


def window(d, b, me):
    """why the window was shared with another holder of the lock, empty when it was not"""
    why = []
    if "others_snap" not in b:
        why.append("lock status at snap unread")
    elif b["others_snap"].split():
        why.append("holding at snap: " + b["others_snap"])
    st = read(d, "status.txt")
    if st is None:
        why.append("lock status at check unread")
    elif holders(st, me):
        why.append("holding at check: " + " ".join(holders(st, me)))
    log = read(d, "lock.log")
    if log is None or "start_epoch" not in b:
        why.append("lock log unread")
    else:
        n = 0
        for l in log.splitlines():
            f = l.split("\t")
            if len(f) > 3 and f[0].isdigit() and int(f[0]) > int(b["start_epoch"]) and f[3] in ("took", "released"):
                n += 1
        if n:
            why.append("%d lock take/release in the window" % n)
    return why


def tcc_check(d, b, under, left, wide, unread):
    tcc = read(d, "tcc.ndjson")
    if tcc is None or '"finished"' not in tcc:
        unread.append("tcc log: log show did not finish")
        return
    reqs = tcc_requests(tcc)
    if not reqs and b.get("tcc_live") != "1":
        unread.append("tcc log: no request in the suite nor in the 24 h probe (predicate or format dead)")
        return
    mine = [r for r in reqs.values() if any(ours(p) for p in r["paths"])]
    print("tcc: requests=%d ours=%d" % (len(reqs), len(mine)))
    for r in mine:
        line = "tcc: %s %s" % (r["svc"], " ".join(r["paths"]))
        (left if any(under(p) for p in r["paths"]) else wide).append(line)


def check(d, before, lrc, lout, me, prefixes):
    left, unread, wide, wide_unread = [], [], [], []
    pairs = [l.split("=", 1) for l in open(before).read().splitlines() if "=" in l] if os.path.exists(before) else []
    b = dict(pairs)
    if not pairs:
        unread.append("no before snapshot")
    known = [k for k in os.environ.get("CLEAN_KNOWN", "").split("\n") if k] + [v for k, v in pairs if k == "known"]
    known = ["%s:%s" % (k.split(":", 1)[0], strip(k.split(":", 1)[1])) for k in known if ":" in k]
    c, recs, why = state(d)
    unread += [w for w in why if w.startswith("lsappinfo")]
    wide_unread += [w for w in why if not w.startswith("lsappinfo")]
    print("before: fg=%s persistent=%s recent=%s start=%s" % tuple(b.get(k, "?") for k in ("fg", "persistent", "recent", "start")))
    print("after:  fg=%s persistent=%s recent=%s" % tuple(c.get(k, "?") for k in ("fg", "persistent", "recent")))
    for k in ("fg", "persistent", "recent"):
        if k not in b:
            wide_unread.append("%s count not read before the suite" % k)
        elif k in c and int(b[k]) != c[k]:
            wide.append("%s count changed: %s before, %s after" % (k, b[k], c[k]))
    dock_check(d, b, wide, wide_unread)
    under = lambda p: any(strip(p).startswith(x) for x in prefixes)
    for r in recs or []:
        if key(r) in known:
            known.remove(key(r))
        elif any(ours(p) for p in r["paths"] + [r["name"]]):
            wide.append("record: %s pid=%s %s" % (r["type"], r["pid"], (r["paths"] or [r["name"]])[0]))
    if lrc == 1:
        left += ["leftover: " + l for l in lout.splitlines()]
    elif lrc != 0:
        unread.append("leftovers: " + (lout.strip() or "exit %d" % lrc))
    tcc_check(d, b, under, left, wide, unread)
    shared = window(d, b, me)
    if shared:
        print("window: shared-window (%s); only this run's paths are judged" % "; ".join(shared))
        print("".join("info: %s\n" % l for l in wide + wide_unread), end="")
    else:
        print("window: alone")
        left, unread = left + wide, unread + wide_unread
    print("".join("left: %s\n" % l for l in left) + "".join("unread: %s\n" % u for u in unread), end="")
    clean = 1 if left else 2 if unread else 0
    print("clean=%d" % clean)
    return clean


if sys.argv[1] == "snap":
    snap(sys.argv[2], sys.argv[3])
    sys.exit(0)
d, before, lrc, lout, me = sys.argv[2:7]
sys.exit(check(d, before, int(lrc), lout, me, [strip(p) for p in sys.argv[7:]]))
