#!/usr/bin/env python3
"""The Claude Code side of the peerbus end-to-end test.

Listens on a socket of its own next to the Codex inbox, the way a
Claude Code session does, writes one Claude-shaped message to the inbox
(optionally followed by an idle subscription, as Claude Code's
SendMessage with notify_when_idle sends it), and waits for the answer.

Usage: _peerbus_claude_side.py <inbox-socket> <seconds> <expected-text> <tag> [notify]

The message carries <tag>, so a test can find it on a screen.  An empty
<expected-text> sends a message that asks for no reply.

Prints what arrived as JSON lines and exits 0 once the reply body
contains <expected-text> and, with `notify`, a peer_idle_notice for the
subscription has come.  Exits 1 on a receipt (the message was refused
or dropped), on a reply without the text, or when time runs out.
"""
import json
import os
import re
import socket
import sys
import time
import uuid

inbox, seconds, expected, tag = sys.argv[1], float(sys.argv[2]), sys.argv[3], sys.argv[4]
notify = len(sys.argv) > 5 and sys.argv[5] == "notify"
me = os.path.join(os.path.dirname(inbox), "e2e-%d-%s.sock" % (os.getpid(), uuid.uuid4().hex[:8]))
listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(me)
listener.listen(4)


def write(frame):
    out = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    out.connect(inbox)
    out.sendall((json.dumps(frame) + "\n").encode())
    out.close()


try:
    sender = "uds:" + me
    ask = " Reply to this message." if expected else ""
    body = "End-to-end test %s of the peerbus bridge.%s" % (tag, ask)
    content = ('<cross-session-message from="%s" from-name="e2e-claude" from-mode="bypass">\n%s\n'
               "</cross-session-message>" % (sender, body))
    write({"msgV": 1, "msg_id": str(uuid.uuid4()), "type": "user",
           "message": {"role": "user", "content": content}, "priority": "next", "from": sender})
    sub_id = str(uuid.uuid4())
    if notify:
        write({"type": "control", "action": "notify_when_idle", "from": sender,
               "from_mode": "bypass", "msgV": 1, "msg_id": sub_id})

    got_reply, got_notice = not expected, not notify
    deadline = time.time() + seconds
    listener.settimeout(1.0)
    while time.time() < deadline and not (got_reply and got_notice):
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
        if msg.get("type") == "control" and msg.get("action") == "peer_idle_notice":
            print(json.dumps({"notice": msg}))
            if msg.get("orig_msg_id") != sub_id or msg.get("state") != "idle":
                sys.exit(1)
            got_notice = True
            continue
        if msg.get("type") != "user":
            print(json.dumps({"receipt": msg}))
            sys.exit(1)
        text = msg["message"]["content"]
        m = re.search(r'<cross-session-message from="([^"]*)"', text)
        print(json.dumps({"from": m.group(1) if m else None, "content": text}))
        if expected not in text:
            sys.exit(1)
        got_reply = True
    if not (got_reply and got_notice):
        print(json.dumps({"timeout": seconds, "reply": got_reply, "notice": got_notice}))
        sys.exit(1)
finally:
    listener.close()
    os.unlink(me)
