#!/usr/bin/env python3
"""A process that registers itself the way a Claude Code session does,
for the peerbus end-to-end test: one record in <registry>, named the
way Claude names a session from its directory, then it waits.

Usage: _peerbus_fake_claude.py <registry-dir> <seconds>

marspot is expected to rename the record after the pane this runs in.
The record is removed when the time is up.
"""
import json
import os
import subprocess
import sys
import time
import uuid

registry, seconds = sys.argv[1], float(sys.argv[2])
pid = os.getpid()
lstart = subprocess.run(["ps", "-o", "lstart=", "-p", str(pid)], env={"LC_ALL": "C", "TZ": "UTC"},
                        capture_output=True, text=True).stdout.strip()
now = int(time.time() * 1000)
path = os.path.join(registry, "%d.json" % pid)
with open(path, "w") as f:
    json.dump({"pid": pid, "sessionId": str(uuid.uuid4()), "cwd": os.getcwd(), "startedAt": now,
               "procStart": lstart, "version": "2.1.308", "peerProtocol": 1,
               "peerFeatures": ["notify_idle"], "kind": "interactive", "entrypoint": "cli",
               "pidDomain": "darwin", "messagingSocketPath": "/tmp/cc-socks/e2e-fake-%d.sock" % pid,
               "name": "fake-claude-6a", "nameSource": "derived", "nameSince": now,
               "status": "idle", "updatedAt": now, "statusUpdatedAt": now}, f)
print("fake claude %d registered" % pid, flush=True)
try:
    time.sleep(seconds)
finally:
    os.unlink(path)
