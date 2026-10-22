#!/usr/bin/env python3
"""The Claude Code side of the peerbus end-to-end test.

Listens on a socket of its own next to the Codex inbox, the way a
Claude Code session does, writes one Claude-shaped message to the inbox,
and waits for the answer.

Usage: _peerbus_claude_side.py <inbox-socket> <seconds> <expected-text> <tag>

The message carries <tag>, so a test can find it on a screen.

Prints the reply address and body of the answer as one JSON line and
exits 0 when the body contains <expected-text>.  Exits 1 on a receipt
(the message was refused or dropped), on an answer without the text,
or when nothing arrives in time.
"""
import json
import os
import re
import socket
import sys
import time
import uuid

inbox, seconds, expected, tag = sys.argv[1], float(sys.argv[2]), sys.argv[3], sys.argv[4]
me = os.path.join(os.path.dirname(inbox), "e2e-%d-%s.sock" % (os.getpid(), uuid.uuid4().hex[:8]))
listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(me)
listener.listen(4)
try:
    sender = "uds:" + me
    body = "End-to-end test %s of the peerbus bridge. Reply to this message." % tag
    content = ('<cross-session-message from="%s" from-name="e2e-claude" from-mode="bypass">\n%s\n'
               "</cross-session-message>" % (sender, body))
    frame = {"msgV": 1, "msg_id": str(uuid.uuid4()), "type": "user",
             "message": {"role": "user", "content": content}, "priority": "next", "from": sender}
    out = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    out.connect(inbox)
    out.sendall((json.dumps(frame) + "\n").encode())
    out.close()

    deadline = time.time() + seconds
    listener.settimeout(1.0)
    while time.time() < deadline:
        try:
            conn, _ = listener.accept()
        except socket.timeout:
            continue
        data = b""
        conn.settimeout(5.0)
        while not data.endswith(b"\n"):
            chunk = conn.recv(65536)
            if not chunk:
                break
            data += chunk
        conn.close()
        if not data.strip():
            continue  # a sender's liveness probe
        msg = json.loads(data.decode().splitlines()[0])
        if msg.get("type") != "user":
            print(json.dumps({"receipt": msg}))
            sys.exit(1)
        text = msg["message"]["content"]
        m = re.search(r'<cross-session-message from="([^"]*)"', text)
        print(json.dumps({"from": m.group(1) if m else None, "content": text}))
        sys.exit(0 if expected in text else 1)
    print(json.dumps({"timeout": seconds}))
    sys.exit(1)
finally:
    listener.close()
    os.unlink(me)
