#!/usr/bin/env python3
"""A process that registers itself the way a Claude Code session does,
for the peerbus end-to-end test: one record in <registry>, named the
way Claude names a session from its directory, and an inbox socket
that takes whatever is written to it.

Usage: _peerbus_fake_claude.py <registry-dir> <seconds> [<log-file>]

Every line that arrives on the inbox is appended to <log-file>.  The
record and the socket are removed when the time is up.
"""
import json
import os
import signal
import socket
import subprocess
import sys
import time
import uuid

registry, seconds = sys.argv[1], float(sys.argv[2])
log = sys.argv[3] if len(sys.argv) > 3 else os.devnull
pid = os.getpid()
lstart = subprocess.run(["ps", "-o", "lstart=", "-p", str(pid)], env={"LC_ALL": "C", "TZ": "UTC"},
                        capture_output=True, text=True).stdout.strip()
now = int(time.time() * 1000)
path = os.path.join(registry, "%d.json" % pid)
inbox = "/tmp/cc-socks/e2e-fake-%d.sock" % pid
os.makedirs(os.path.dirname(inbox), exist_ok=True)
listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(inbox)
listener.listen(8)
listener.settimeout(0.5)
with open(path, "w") as f:
    json.dump({"pid": pid, "sessionId": str(uuid.uuid4()), "cwd": os.getcwd(), "startedAt": now,
               "procStart": lstart, "version": "2.1.308", "peerProtocol": 1,
               "peerFeatures": ["notify_idle"], "kind": "interactive", "entrypoint": "cli",
               "pidDomain": "darwin", "messagingSocketPath": inbox,
               "name": "fake-claude-6a", "nameSource": "derived", "nameSince": now,
               "status": "idle", "updatedAt": now, "statusUpdatedAt": now}, f)
print("fake claude %d registered" % pid, flush=True)
# a kill still removes the record and the socket
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
deadline = time.time() + seconds
try:
    while time.time() < deadline:
        try:
            conn, _ = listener.accept()
        except socket.timeout:
            continue
        conn.settimeout(5.0)
        data = b""
        try:
            while True:
                chunk = conn.recv(65536)
                if not chunk:
                    break
                data += chunk
        except socket.timeout:
            pass
        conn.close()
        with open(log, "ab") as f:
            f.write(data)
finally:
    os.unlink(path)
    os.unlink(inbox)
