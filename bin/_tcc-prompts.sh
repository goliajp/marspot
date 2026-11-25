#!/bin/bash
# bin/_tcc-prompts.sh START BUNDLE_ID [PATH_PREFIX] — the system permission
# prompts asked on behalf of an app since START ("YYYY-MM-DD HH:MM:SS",
# local time), read from the unified log.  Prints one line per prompt
# (time, service) and last the count alone.
#
# tccd logs AUTHREQ_PROMPTING when it decides to ask the user, naming the
# subject and the responsible process.  A prompt counts as the app's when
# the subject is BUNDLE_ID exactly, or the responsible process runs from
# under PATH_PREFIX: a pane's shell asking for a folder is attributed to
# the app that launched it.  This is the decision to ask, not the dialog:
# on a host whose prompt windows are stuck behind older unanswered ones
# no window appears, and the count is still right.  A request that the
# user's TCC database already answers, allowed or denied, logs no prompt.
set -uo pipefail
start="${1:?usage: $0 START BUNDLE_ID [PATH_PREFIX]}"
bundle="${2:?usage: $0 START BUNDLE_ID [PATH_PREFIX]}"
prefix="${3:-}"
/usr/bin/log show --start "$start" --style ndjson \
  --predicate 'subsystem == "com.apple.TCC" AND eventMessage BEGINSWITH "AUTHREQ_PROMPTING"' 2>/dev/null |
  /usr/bin/python3 -c '
import json, re, sys
bundle, prefix = sys.argv[1], sys.argv[2]
seen = {}
for line in sys.stdin:
    try:
        e = json.loads(line)
    except ValueError:
        continue
    msg = e.get("eventMessage", "")
    if not msg.startswith("AUTHREQ_PROMPTING"):
        continue
    ours = ("Sub:{%s}" % bundle) in msg or (prefix and ("responsible_path=" + prefix) in msg)
    if not ours:
        continue
    mid = re.search(r"msgID=([^,]+)", msg)
    svc = re.search(r"service=(\w+)", msg)
    seen.setdefault(mid.group(1) if mid else msg, (e.get("timestamp", "?")[:19], svc.group(1) if svc else "?"))
for t, svc in seen.values():
    print(f"{t}  {svc}")
print(len(seen))
' "$bundle" "$prefix"
