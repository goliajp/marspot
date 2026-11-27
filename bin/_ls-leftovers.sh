#!/bin/bash
# bin/_ls-leftovers.sh [--known PID:PATH]... PATH_PREFIX... — the
# applications LaunchServices still lists whose executable or bundle is
# under one of the prefixes: what a run left in the Dock (a Foreground
# record is a Dock tile, alive or not) and in the app list.  Read-only
# (`lsappinfo list`).
#
# Prints one line per record ("<type> pid=<pid> <bundle or executable>")
# and last "left=N tiles=T known=K", T the Foreground ones.  A record
# matching a --known PID:PATH (the pid and path of one of those lines,
# taken earlier) is not listed and counts in K instead, once per time it
# was given.  Exit 0 when nothing else is left, 1 when something is, 2
# when lsappinfo could not be read.
# Run outside the console login session (a plain ssh), lsappinfo answers
# with a list that has none of the login's apps and so none of ours
# either: a list without the Dock is taken as unread.
set -uo pipefail
known=""
while [[ ${1:-} == --known && $# -ge 2 ]]; do
  known+="$2"$'\n'
  shift 2
done
(( $# > 0 )) || { echo "usage: $0 [--known PID:PATH]... PATH_PREFIX..." >&2; exit 2; }
out="$(/usr/bin/lsappinfo list 2>/dev/null)" || exit 2
[[ -n $out ]] || exit 2
printf '%s\n' "$out" | LS_KNOWN="$known" /usr/bin/python3 -c '
import collections, os, re, sys
strip = lambda p: p[len("/private"):] if p.startswith("/private/") else p
prefixes = [strip(p) for p in sys.argv[1:]]
recs, cur = [], None
for line in sys.stdin:
    if re.match(r"\s*\d+\) \"", line):
        cur = {"type": "?", "pid": "?", "paths": []}
        recs.append(cur)
        continue
    if cur is None:
        continue
    for key in ("bundle path", "executable path"):
        m = re.search(key + r"=\"([^\"]*)\"", line)
        if m:
            cur["paths"].append(strip(m.group(1)))
    m = re.search(r"type=\"(\w+)\"", line)
    if m:
        cur["type"] = m.group(1)
    m = re.search(r"pid = (-?\d+)", line)
    if m:
        cur["pid"] = m.group(1)
# by path: the names are in the login language
if not any(p.startswith("/System/Library/CoreServices/Dock.app") for r in recs for p in r["paths"]):
    print("lsappinfo does not list the Dock: not the list of the console login")
    sys.exit(2)
known = collections.Counter(k for k in os.environ["LS_KNOWN"].split("\n") if k)
left, nknown = [], 0
for r in recs:
    if not any(p.startswith(x) for p in r["paths"] for x in prefixes):
        continue
    key = "%s:%s" % (r["pid"], r["paths"][0])
    if known[key] > 0:
        known[key] -= 1
        nknown += 1
    else:
        left.append(r)
for r in left:
    print("%s pid=%s %s" % (r["type"], r["pid"], r["paths"][0]))
tiles = sum(r["type"] == "Foreground" for r in left)
print("left=%d tiles=%d known=%d" % (len(left), tiles, nknown))
sys.exit(1 if left else 0)
' "$@"
