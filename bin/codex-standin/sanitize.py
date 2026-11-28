#!/usr/bin/env python3
"""Turns a raw recording (record.py) into the checked-in fixture: the
scratch paths, the thread ids, the model stand-in's port and the user's
name become placeholders the stand-in fills back in.  Refuses when a
path of the recording machine is left.

Usage: sanitize.py <raw.json> <scratch-dir> <codex-version> <out.json>
"""
import getpass
import json
import os
import re
import sys

RAW, SB, VERSION, OUT = sys.argv[1:5]
text = open(RAW).read()
rec = json.loads(text)

threads = set()
for p in rec["probes"]:
    for t in p.get("thread/loaded/list", {}).get("result", {}).get("data", []):
        threads.add(t)
if not threads:
    sys.exit("the recording has no thread: %s" % json.dumps(rec["probes"])[:400])

pairs = [(os.path.join(SB, "home", ".codex"), "{CODEX_HOME}"), (os.path.join(SB, "proj"), "{CWD}"),
         (os.path.join(SB, "home"), "{HOME}"), (SB, "{SB}")]
port = re.search(r"127\.0\.0\.1:(\d+)", text)
for i, t in enumerate(sorted(threads)):
    pairs.append((t, "{THREAD}" if i == 0 else "{THREAD%d}" % i))
    # the title shows the start of the id
    pairs.append((t[:8], "{THREAD_HEAD}" if i == 0 else "{THREAD%d_HEAD}" % i))
out = json.dumps(rec, indent=1, ensure_ascii=False)
for old, new in pairs:
    out = out.replace(old, new)
if port:
    out = out.replace("127.0.0.1:" + port.group(1), "127.0.0.1:{PORT}")
out = out.replace(VERSION, "{VERSION}")
user = getpass.getuser()
for leak in ("/Users/", "/private/var/", "/var/folders/", user):
    if leak in out:
        at = out.index(leak)
        sys.exit("the recording still holds %r: ...%s..." % (leak, out[max(0, at - 60):at + 60]))
with open(OUT, "w") as f:
    f.write(out + "\n")
