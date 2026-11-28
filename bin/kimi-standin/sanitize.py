#!/usr/bin/env python3
"""Turns a raw recording (record.py) into the checked-in fixture.

Replaced by placeholders the stand-in fills back in: the scratch paths
(also the shortened form the footer shows), the session id and its
workspace directory, the model stand-in's port, the host name and the
pid in the lock, and the time zone in the date reminder.  Records over 2 KB (the tool lists kimi snapshots)
are left out, and the system prompt of `profile.bind` is cut to `…`.
Refuses when a path of the recording machine, the user's name or the
host name is left, or the value of a secret-looking variable of this
environment (a name with KEY, TOKEN or SECRET, longer than 8 chars).

Usage: sanitize.py <raw.json> <scratch-dir> <out.json>
"""
import getpass
import json
import os
import re
import socket
import sys

RAW, SB, OUT = sys.argv[1:4]
rec = json.load(open(RAW))


def short(line):
    if '"type":"profile.bind"' in line:
        v = json.loads(line)
        v["systemPrompt"] = "…"
        return json.dumps(v, separators=(",", ":"), ensure_ascii=False)
    return line if len(line) <= 2048 else None


rec["wire"] = "".join(s + "\n" for s in map(short, rec["wire"].splitlines()) if s)
sessions = sorted(set(re.findall(r"session_[0-9a-f-]{36}", json.dumps(rec))))
workspaces = sorted(set(re.findall(r"wd_[A-Za-z0-9]+_[0-9a-f]{12}", json.dumps(rec))))
if len(sessions) != 1 or len(workspaces) != 1:
    sys.exit("expected one session and one workspace, found %s %s" % (sessions, workspaces))
lock = next(iter(rec["locks"].values()), None)
if lock is None:
    sys.exit("no lock was seen during a turn")
pid = str(json.loads(lock)["pid"])
host = json.loads(lock)["hostname"]
for path, text in rec["locks"].items():
    text = text.replace('"pid":%s,' % pid, '"pid":{PID},').replace('"token":"%s:' % pid, '"token":"{PID}:')
    rec["locks"][path] = text.replace('"hostname":"%s"' % host, '"hostname":"{HOST}"')

out = json.dumps(rec, indent=1, ensure_ascii=False)
pairs = [(os.path.join(SB, "home", ".kimi-code"), "{KIMI_HOME}"), (os.path.join(SB, "proj"), "{CWD}"),
         (os.path.join(SB, "home"), "{HOME}"), (SB, "{SB}"), (sessions[0], "{SESSION}"),
         (workspaces[0], "{WORKSPACE}")]
for old, new in pairs:
    out = out.replace(old, new)
# the date reminder names the machine's time zone
zone = os.path.realpath("/etc/localtime").split("zoneinfo/")[-1]
if "/" in zone:
    out = out.replace(zone, "{TIME_ZONE}")
# the footer shows the directory's last components after `…`
out = out.replace("/tmp/" + os.path.basename(SB), "{SB_TAIL}")
port = re.search(r"127\.0\.0\.1:(\d+)", out)
if port:
    out = out.replace("127.0.0.1:" + port.group(1), "127.0.0.1:{PORT}")
for leak in ("/Users/", "/private/", "/var/folders/", "/tmp/", os.path.basename(SB), getpass.getuser(),
             socket.gethostname().split(".")[0], host, pid):
    m = re.search(r"(?<![0-9A-Za-z])%s(?![0-9A-Za-z])" % re.escape(leak), out)
    if m:
        at = m.start()
        sys.exit("the recording still holds %r: ...%s..." % (leak, out[max(0, at - 60):at + 60]))
# the recording ran in a clean environment; this shell's secrets must
# still not be in it
for name, value in os.environ.items():
    if re.search(r"KEY|TOKEN|SECRET", name) and len(value) > 8 and value in out:
        sys.exit("the recording holds the value of $%s" % name)
with open(OUT, "w") as f:
    f.write(out + "\n")
