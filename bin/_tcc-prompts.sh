#!/bin/bash
# bin/_tcc-prompts.sh START PATH_PREFIX — the system permission requests
# tccd handled for one copy of the app since START ("YYYY-MM-DD HH:MM:SS",
# local time), and which of them it decided to ask the user about, read
# from the unified log.
#
# A request is the app's when its attribution names a process running
# from under PATH_PREFIX, as the responsible process or the requesting
# one: a pane's shell asking for a folder is attributed to the app that
# launched it.  The path, not the bundle id, because other copies of the
# app with the same id may be running on the host at the same time.
#
# Each request is one of:
#   prompt  tccd logged AUTHREQ_PROMPTING: it decided to ask.  This is the
#           decision, not the dialog: on a host whose prompt windows are
#           stuck behind older unanswered ones no window appears, and the
#           count is still right
#   stored  answered without asking from a decision the login user's TCC
#           database holds (authReason 2 user consent, 3 user set, 4
#           system set, 6 MDM, 7 override): a Mac without that decision
#           might have asked
#   policy  answered without asking by something the same on every Mac
#           (no record and only a preflight, service policy, entitlement)
#
# Prints one line per request that is not policy ("prompt|stored <time>
# <service> ...") and last "requests=N prompts=P stored=S".
# Exit 0 when it could read the log; 2 when it cannot see: `log show`
# failed, tccd logged no request from any process in the window (the
# witness that the log is readable at all), or an attribution was
# redacted, in which case a request of the app's could hide behind it.
set -uo pipefail
start="${1:?usage: $0 START PATH_PREFIX}"
prefix="${2:?usage: $0 START PATH_PREFIX}"
/usr/bin/log show --start "$start" --style ndjson \
  --predicate 'process == "tccd" AND eventMessage BEGINSWITH "AUTHREQ_"' |
  /usr/bin/python3 -c '
import json, re, sys
prefix = sys.argv[1]
ours_re = re.compile(r"(?:responsible_path|binary_path)=" + re.escape(prefix))
reqs = {}
redacted = False
for line in sys.stdin:
    try:
        e = json.loads(line)
    except ValueError:
        continue
    msg = e.get("eventMessage", "")
    if "<private>" in msg:
        redacted = True
    mid = re.search(r"msgID=([^,]+)", msg)
    if not mid:
        continue
    r = reqs.setdefault(mid.group(1), {"t": e.get("timestamp", "?")[:19], "ours": False,
                                       "prompt": False, "svc": "?", "result": None})
    if ours_re.search(msg):
        r["ours"] = True
    if msg.startswith("AUTHREQ_PROMPTING"):
        r["prompt"] = True
    svc = re.search(r"service=(\w+)", msg)
    if svc:
        r["svc"] = svc.group(1)
    res = re.search(r"authValue=(\d+), authReason=(\d+)", msg)
    if res:
        r["result"] = (int(res.group(1)), int(res.group(2)))
ours = [r for r in reqs.values() if r["ours"]]
prompts = stored = 0
for r in ours:
    t, svc = r["t"], r["svc"]
    if r["prompt"]:
        prompts += 1
        print(f"prompt {t}  {svc}")
    elif r["result"] and r["result"][1] in (2, 3, 4, 6, 7):
        stored += 1
        v, why = r["result"]
        print(f"stored {t}  {svc}  authValue={v} authReason={why}")
print(f"requests={len(ours)} prompts={prompts} stored={stored} witness={len(reqs)}")
sys.exit(2 if redacted or not reqs else 0)
' "$prefix"
st=("${PIPESTATUS[@]}")
if [[ ${st[0]} != 0 ]]; then
  echo "log show exited ${st[0]}: cannot see" >&2
  exit 2
fi
if [[ ${st[1]} == 2 ]]; then
  echo "no readable tccd request from any process since $start: cannot see" >&2
fi
exit "${st[1]}"
