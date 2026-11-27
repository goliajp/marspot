#!/bin/bash
# bin/_ls-leftovers.sh PATH_PREFIX... — the applications LaunchServices
# still lists whose executable or bundle is under one of the prefixes:
# what a run left in the Dock (a Foreground record is a Dock tile, alive
# or not) and in the app list.  Read-only (`lsappinfo list`).
#
# Prints one line per record ("<type> pid=<pid> <bundle or executable>")
# and last "left=N tiles=T", T the Foreground ones.  Exit 0 when nothing
# is left, 1 when something is, 2 when lsappinfo could not be read.
set -uo pipefail
(( $# > 0 )) || { echo "usage: $0 PATH_PREFIX..." >&2; exit 2; }
out="$(/usr/bin/lsappinfo list 2>/dev/null)" || exit 2
[[ -n $out ]] || exit 2
printf '%s\n' "$out" | /usr/bin/python3 -c '
import re, sys
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
left = [r for r in recs if any(p.startswith(x) for p in r["paths"] for x in prefixes)]
for r in left:
    print("%s pid=%s %s" % (r["type"], r["pid"], r["paths"][0]))
tiles = sum(r["type"] == "Foreground" for r in left)
print("left=%d tiles=%d" % (len(left), tiles))
sys.exit(1 if left else 0)
' "$@"
