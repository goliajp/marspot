# bin/_e2e-suite-clean.py snap DIR LOCK_PID
#                         check DIR BEFORE LEFTOVERS_RC LEFTOVERS_OUT LOCK_PID PREFIX...
# the decision of bin/_e2e-suite-clean.sh over the inputs it saved in DIR.
# snap prints the before state as key=value lines; check prints the raw
# counts, what was found and last clean=0|1|2 (also the exit status).
# CLEAN_KNOWN holds PID:PATH records listed before the run, one per line.
import collections, json, os, re, sys, urllib.parse
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


def tile_entries(text):
    """the Dock's tiles as [path, bundle id, label]"""
    out = []
    for chunk in text.split('"tile-data"')[1:]:
        f = []
        for k in ("_CFURLString", "bundle-identifier", "file-label"):
            m = re.search(r'"%s" = (?:"((?:[^"\\]|\\.)*)"|([^;\s]*));' % k, chunk)
            f.append((m.group(1) or m.group(2) or "") if m else "")
        if f[0].startswith("file://"):
            f[0] = strip(urllib.parse.unquote(f[0][len("file://"):]))
        out.append(f)
    return out


def ident(e):
    return e[0] or e[1] or "label " + e[2]


def state(d):
    """the counts read now and the records, or None for the list when it is not the console login's;
    the kept and recent tiles as lists"""
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
            c[k] = tile_entries(t)
    return c, recs, why


def tile_lines(text):
    return [l for l in (text or "").splitlines() if INSERT in l or REMOVE in l]


def tcc_requests(text):
    """per msgID: every service, CTX preflight value and desired_auth seen, so two requests that
    share a msgID (pid.counter wraps) can never pass as one preflight; a prompt line without a
    msgID is kept under its own key"""
    reqs, seen = {}, {}
    for n, line in enumerate((text or "").splitlines()):
        try:
            msg = json.loads(line).get("eventMessage", "")
        except (ValueError, AttributeError):
            continue
        m = re.search(r"msgID=([^,]+)", msg)
        if not m and not msg.startswith("AUTHREQ_PROMPTING"):
            continue
        # a client may reuse a msgID for its next request: each CTX line starts a new one, and
        # the lines after it attach to the newest
        key = m.group(1) if m else "!prompt-%d" % n
        if msg.startswith("AUTHREQ_CTX") and key in seen:
            seen[key] += 1
        seen.setdefault(key, 0)
        key = "%s#%d" % (key, seen[key])
        r = reqs.setdefault(key, {"svcs": set(), "paths": [], "ctx": [], "prompt": False, "desired": set()})
        r["svcs"].update(re.findall(r"\bservice=(\w+)", msg))
        if msg.startswith("AUTHREQ_CTX"):
            r["ctx"].append(re.findall(r", preflight=(\w+),", msg))
        if msg.startswith("AUTHREQ_PROMPTING"):
            r["prompt"] = True
        r["desired"].update(int(a) for a in re.findall(r"\bdesired_auth=(\d+)", msg))
        # responsible, accessing and requesting each name a binary_path
        for p in re.findall(r"(?:binary|responsible)_path=([^,}]+)", msg):
            if strip(p) not in r["paths"]:
                r["paths"].append(strip(p))
    for k, r in reqs.items():
        r["svc"] = ",".join(sorted(r["svcs"])) or "?"
        # interleaved requests under one msgID may leave a CTX without its own paths: it takes
        # every path seen under that msgID, so a request of ours is never dropped as not ours
        for o, q in reqs.items():
            if o != k and o.split("#")[0] == k.split("#")[0]:
                r["paths"] += [p for p in q["paths"] if p not in r["paths"]]
    return reqs


QUIET_SERVICES = {"kTCCServiceListenEvent", "kTCCServiceScreenCapture"}

# seconds before the snap in which our own take of the lock may be logged
LOOKBACK = 5


def holders(text, me):
    return [m.group(1) for m in re.finditer(r"(?m)^(\d+)\t(?:heavy|bench)\tholding", text) if m.group(1) != me]


def self_cmd(text, me):
    """this holder's command as the lock log writes it (tabs and newlines as spaces, 200 chars):
    the log lines carry no pid, so the command is how our own took line is told apart"""
    for line in (text or "").splitlines():
        f = line.split("\t", 3)
        if me and len(f) == 4 and f[0] == me and f[2].startswith("holding"):
            cmd = f[3][: -len(" [stopped]")] if f[3].endswith(" [stopped]") else f[3]
            return cmd.replace("\t", " ").replace("\n", " ")[:200]
    return None


def snap(d, me):
    c, recs, why = state(d)
    out = ["fg=%d" % c["fg"]] if "fg" in c else []
    for k in ("persistent", "recent"):
        if k in c:
            out += ["%s=%d" % (k, len(c[k])), "%s_entries=%s" % (k, json.dumps(c[k]))]
    out += ["unread=" + w for w in why]
    out += ["known=" + key(r) for r in recs or [] if any(ours(p) for p in r["paths"] + [r["name"]])]
    st = read(d, "status.txt")
    if st is not None:
        out.append("others_snap=" + " ".join(holders(st, me)))
        own = self_cmd(st, me)
        if own is not None:
            out.append("self_cmd=" + own)
    if read(d, "probe-dock.log") is not None or read(d, "probe-tcc.ndjson") is not None:
        out.append("dock_live=%d" % bool(tile_lines(read(d, "probe-dock.log"))))
        out.append("tcc_live=%d" % bool(tcc_requests(read(d, "probe-tcc.ndjson"))))
    print("\n".join(out))


def dock_count(lines):
    """inserts and removals by reason, and removal events: a removal line right after a
    removal line of another reason within a second is the same removal logged twice,
    which only ever undercounts removals.  Every line that is not an insert counts as a
    removal: one by an unrelated app while we hold the lock alone can offset one of our
    inserts here, and the lsappinfo record check is what catches our app then"""
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
    """alone, shared (another holder seen in the lock status or log) or unknown (a source
    unread and no other holder seen), and why"""
    seen, unread = [], []
    if "others_snap" not in b:
        unread.append("lock status at snap unread")
    elif b["others_snap"].split():
        seen.append("holding at snap: " + b["others_snap"])
    st = read(d, "status.txt")
    if st is None:
        unread.append("lock status at check unread")
    elif holders(st, me):
        seen.append("holding at check: " + " ".join(holders(st, me)))
    log = read(d, "lock.log")
    if log is None or "start_epoch" not in b:
        unread.append("lock log unread")
    else:
        start, own = int(b["start_epoch"]), b.get("self_cmd") or self_cmd(st, me)
        lines = [f for f in (l.split("\t") for l in log.splitlines())
                 if len(f) > 4 and f[0].isdigit() and f[3] in ("took", "released")]
        # our own took line comes before the snap: in its second or a few before
        mine = next((i for i, f in enumerate(lines) if own is not None and f[3] == "took"
                     and f[4] == own and start - LOOKBACK <= int(f[0]) <= start), None)
        found = own is None or mine is not None
        n, odd = 0, 0
        for i, f in enumerate(lines):
            t = int(f[0])
            if i == mine or t < start:
                continue
            # a take in the start second that is not known to be ours may be ours
            if t == start and f[3] == "took" and not (own is not None and found):
                odd += 1
                continue
            n += 1
        if n:
            seen.append("%d lock take/release in the window" % n)
        if odd:
            unread.append("%d lock take in the start second, not told apart from ours" % odd)
        if not found:
            # a take we cannot tell from ours may be ours: counting it as another
            # holder would make the window shared and the machine-wide checks info
            unread.append("our own lock take not found in the lock log")
    if seen:
        return "shared", seen + unread
    return ("unknown", unread) if unread else ("alone", [])


def tiles_check(b, c, under, left, wide, wide_unread):
    """each kept or recent tile added, and each kept tile gone: the lists have a cap, so
    a new entry can push the oldest out and leave the count as it was"""
    for k in ("persistent", "recent"):
        if k + "_entries" not in b:
            wide_unread.append("%s tiles not read before the suite" % k)
            continue
        if k not in c:
            continue
        was = collections.Counter(ident(e) for e in json.loads(b[k + "_entries"]))
        now = collections.Counter(ident(e) for e in c[k])
        shown = {ident(e): e for e in c[k]}
        for i in now - was:
            e = shown[i]
            mine = any(p and (under(p) or ours(p)) for p in e)
            (left if mine else wide).append("%s tile added: %s %s" % (k, i, " ".join(e[1:])))
        for i in was - now:
            line = "%s tile gone: %s" % (k, i)
            if k == "persistent":
                wide.append(line)
            else:
                print("dock: " + line)


def tcc_check(d, b, under, left, wide, unread):
    tcc = read(d, "tcc.ndjson")
    if tcc is None or '"finished"' not in tcc:
        unread.append("tcc log: log show did not finish")
        return
    reqs = tcc_requests(tcc)
    if not reqs and b.get("tcc_live") != "1":
        unread.append("tcc log: no request in the suite nor in the 24 h probe (predicate or format dead)")
        return
    mine = [r for k, r in reqs.items() if k.startswith("!") or any(ours(p) for p in r["paths"])]
    quiet = [r for r in mine if quiet_preflight(r)]
    count = lambda svc: sum(1 for r in quiet if r["svcs"] == {svc})
    print("tcc: requests=%d ours=%d listen_event_preflights=%d screen_capture_preflights=%d" % (
        len(reqs), len(mine), count("kTCCServiceListenEvent"), count("kTCCServiceScreenCapture")))
    for r in mine:
        line = "tcc: %s %s" % (r["svc"], " ".join(r["paths"]))
        if r in quiet:
            print("info: " + line + " (preflight, no prompt)")
            continue
        (left if r["prompt"] or any(under(p) for p in r["paths"]) else wide).append(line)


def quiet_preflight(r):
    """the input-monitoring and screen-recording preflights AppKit and the window server make
    (the latter before the shell captures its own window): they ask, never prompt and record
    nothing; any other request is a finding"""
    return (len(r["svcs"]) == 1 and r["svcs"] <= QUIET_SERVICES and r["ctx"] == [["yes"]]
            and not r["prompt"] and r["desired"] == {0})


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
    n = lambda k: len(c[k]) if k in c else "?"
    print("after:  fg=%s persistent=%s recent=%s" % (c.get("fg", "?"), n("persistent"), n("recent")))
    if "fg" not in b:
        wide_unread.append("fg count not read before the suite")
    elif "fg" in c and int(b["fg"]) != c["fg"]:
        wide.append("fg count changed: %s before, %s after" % (b["fg"], c["fg"]))
    under = lambda p: any(strip(p).startswith(x) for x in prefixes)
    tiles_check(b, c, under, left, wide, wide_unread)
    dock_check(d, b, wide, wide_unread)
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
    kind, why = window(d, b, me)
    if kind == "shared":
        print("window: shared-window (%s); only this run's paths are judged" % "; ".join(why))
        print("".join("info: %s\n" % l for l in wide + wide_unread), end="")
    elif kind == "unknown":
        print("window: unknown (%s); the machine-wide findings cannot be judged" % "; ".join(why))
        unread += wide + wide_unread
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
