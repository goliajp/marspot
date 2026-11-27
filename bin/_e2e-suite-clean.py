# bin/_e2e-suite-clean.py snap DIR | check DIR BEFORE LEFTOVERS_RC LEFTOVERS_OUT
# the decision of bin/_e2e-suite-clean.sh over the inputs it saved in DIR:
# snap prints the before counts, check the raw counts, what was found and
# last clean=0|1|2 (also the exit status); CLEAN_KNOWN holds the PID:PATH
# records listed before the run, one per line
import json, os, re, sys
def read(d, n):
    p = os.path.join(d, n)
    return open(p, errors="replace").read() if os.path.exists(p) else None
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
        for key in ("bundle path", "executable path"):
            p = re.search(key + r'="([^"]*)"', line)
            if p:
                cur["paths"].append(p.group(1))
        t = re.search(r'type="(\w+)"', line)
        if t:
            cur["type"] = t.group(1)
        p = re.search(r"pid = (-?\d+)", line)
        if p:
            cur["pid"] = p.group(1)
    return recs
def ours(s):
    return "marspot" in s.lower() or "/msp-" in s
def counts(d, unread):
    c, recs = {}, []
    ls = read(d, "lsappinfo.txt")
    if ls is not None:
        recs = records(ls)
    if not any(p.startswith(("/System/Library/CoreServices/Dock.app", "/private/System/Library/CoreServices/Dock.app"))
               for r in recs for p in r["paths"]):
        unread.append("lsappinfo: no list of the console login (no Dock in it)")
    else:
        c["fg"] = sum(r["type"] == "Foreground" for r in recs)
    for k, n in (("persistent", "persistent.txt"), ("recent", "recent.txt")):
        t = read(d, n)
        if t is None:
            unread.append("defaults: com.apple.dock could not be read")
        else:
            c[k] = t.count('"file-label"')
    return c, recs
mode, d = sys.argv[1], sys.argv[2]
unread, left = [], []
c, recs = counts(d, unread)
if mode == "snap":
    print("\n".join("%s=%s" % kv for kv in c.items()))
    print("".join("unread=%s\n" % u for u in unread), end="")
    sys.exit(0)
b = {"unread": "no before snapshot"}
if os.path.exists(sys.argv[3]):
    b = dict(l.split("=", 1) for l in open(sys.argv[3]).read().splitlines() if "=" in l)
unread += ["before: " + v for k, v in b.items() if k == "unread"]
print("before: fg=%s persistent=%s recent=%s start=%s" % tuple(b.get(k, "?") for k in ("fg", "persistent", "recent", "start")))
print("after:  fg=%s persistent=%s recent=%s" % tuple(c.get(k, "?") for k in ("fg", "persistent", "recent")))
for k in ("fg", "persistent", "recent"):
    if k in b and k in c and int(b[k]) != c[k]:
        left.append("%s count changed: %s before, %s after" % (k, b[k], c[k]))
dock = read(d, "dock.log")
if dock is None or not dock.startswith("Timestamp"):
    unread.append("dock log: log show gave no output")
else:
    ins = sum("Inserting tile" in l and "_handleLaunchNotification" in l for l in dock.splitlines())
    rem = sum("Removing tiles identical to" in l and "handleDockEventTileRemoved" in l for l in dock.splitlines())
    other = sum("Removing tiles identical to" in l for l in dock.splitlines()) - rem
    print("dock: inserts=%d removals=%d other_removals=%d" % (ins, rem, other))
    if ins > rem:
        left.append("dock: %d launch tile insert(s), %d removal(s)" % (ins, rem))
known = [k for k in os.environ.get("CLEAN_KNOWN", "").split("\n") if k]
for r in recs:
    key = "%s:%s" % (r["pid"], r["paths"][0] if r["paths"] else "")
    if key in known:
        known.remove(key)
    elif any(ours(p) for p in r["paths"] + [r["name"]]):
        left.append("record: %s pid=%s %s" % (r["type"], r["pid"], (r["paths"] or [r["name"]])[0]))
lrc, lout = int(sys.argv[4]), sys.argv[5]
if lrc == 1:
    left += ["leftover: " + l for l in lout.splitlines()]
elif lrc != 0:
    unread.append("leftovers: " + (lout.strip() or "exit %d" % lrc))
tcc = read(d, "tcc.ndjson")
if tcc is None or '"finished"' not in tcc:
    unread.append("tcc log: log show did not finish")
else:
    reqs = {}
    for line in tcc.splitlines():
        try:
            msg = json.loads(line).get("eventMessage", "")
        except ValueError:
            continue
        m = re.search(r"msgID=([^,]+)", msg)
        if not m:
            continue
        r = reqs.setdefault(m.group(1), {"svc": "?", "req": "?", "resp": "?"})
        for f, pat in (("svc", r"service=(\w+)"), ("req", r"binary_path=([^,}]+)"), ("resp", r"responsible_path=([^,}]+)")):
            v = re.search(pat, msg)
            if v and (f != "req" or r["req"] == "?"):
                r[f] = v.group(1)
    mine = [r for r in reqs.values() if ours(r["req"]) or ours(r["resp"])]
    print("tcc: requests=%d ours=%d" % (len(reqs), len(mine)))
    left += ["tcc: %s req=%s resp=%s" % (r["svc"], r["req"], r["resp"]) for r in mine]
print("".join("left: %s\n" % l for l in left) + "".join("unread: %s\n" % u for u in unread), end="")
clean = 1 if left else 2 if unread else 0
print("clean=%d" % clean)
sys.exit(clean)
