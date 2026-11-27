#!/bin/bash
# bin/_ls-checkins.sh START [END] — the processes that checked in with
# LaunchServices on this host (registered as an app) between START and
# END ("YYYY-MM-DD HH:MM:SS", local time), counted by executable, most
# first, then "total=N".  Read-only: the system log, nothing else.
#
# Hash-named build outputs (`deps/marspot-<hash>`,
# `build/marspot-<hash>/out/marspot-<hash>`) are counted under one name
# each.  Exit 2 when the log could not be
# read at all, which is not the same as nobody checking in: the line
# count of what was read is printed as "read=N" for that reason.
set -uo pipefail
start="${1:?usage: $0 START [END]}"
args=(--start "$start")
[[ -n ${2:-} ]] && args+=(--end "$2")
/usr/bin/log show "${args[@]}" --style ndjson \
  --predicate 'subsystem == "com.apple.processmanager" AND eventMessage BEGINSWITH "CHECKIN: pid="' |
  /usr/bin/python3 -c '
import collections, json, re, sys
c, read = collections.Counter(), 0
for line in sys.stdin:
    read += 1
    try:
        e = json.loads(line)
    except ValueError:
        continue
    if not e.get("eventMessage", "").startswith("CHECKIN: pid="):
        continue
    p = e.get("processImagePath", "?")
    p = re.sub(r"/marspot-[0-9a-f]{16}/out/marspot-[0-9a-f]{16}$", "/marspot-<h>/out/marspot-<h>", p)
    p = re.sub(r"-[0-9a-f]{16}$", "-<h>", p)
    c[p] += 1
for p, n in c.most_common():
    print("%6d %s" % (n, p))
print("total=%d read=%d" % (sum(c.values()), read))
sys.exit(0 if read else 2)
'
