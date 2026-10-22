#!/usr/bin/env python3
"""A stand-in for the model behind Codex, for the peerbus end-to-end test.

Speaks just enough of the Responses API (POST <base>/responses, answered
as server-sent events) for a Codex TUI to run a turn without an account.
When the latest user message carries a reply line of the form
`(Reply with: marspot-peer send <addr> "<text>")`, the turn calls the
shell tool to run that command with REPLY_TEXT in place of <text>; once
the tool has run, the turn ends with a short message.  Anything else is
answered with a short message.

Usage: _peerbus_mock_model.py <port-file> <log-file>
Binds an ephemeral port on 127.0.0.1 and writes it to <port-file>.
Every request body is appended to <log-file>, one JSON line each.
"""
import json
import re
import shlex
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REPLY_TEXT = "PEERBUS-E2E-REPLY"
REPLY_LINE = re.compile(r'\(Reply with: marspot-peer send (\S+) "<text>"\)')

PORT_FILE, LOG_FILE = sys.argv[1], sys.argv[2]
counter = [0]


def next_id(prefix):
    counter[0] += 1
    return "%s_%d" % (prefix, counter[0])


def text_of(item):
    parts = item.get("content") or []
    if isinstance(parts, str):
        return parts
    return "".join(p.get("text", "") for p in parts if isinstance(p, dict))


def shell_call(tools, command):
    """The function_call item that runs `command` with whichever shell
    tool this Codex offers."""
    names = {t.get("name") for t in tools if isinstance(t, dict)}
    if "exec_command" in names:
        return "exec_command", {"cmd": command}
    if "shell_command" in names:
        return "shell_command", {"command": command}
    return "shell", {"command": ["bash", "-lc", command]}


def plan(body):
    items = [i for i in body.get("input") or [] if isinstance(i, dict)]
    last = items[-1] if items else {}
    if last.get("type") == "function_call_output":
        return ("message", "reply sent")
    users = [i for i in items if i.get("type") == "message" and i.get("role") == "user"]
    m = REPLY_LINE.search(text_of(users[-1])) if users else None
    if m and last.get("type") == "message":
        command = "marspot-peer send %s %s" % (m.group(1), shlex.quote(REPLY_TEXT))
        name, args = shell_call(body.get("tools") or [], command)
        return ("call", name, args)
    return ("message", "ok")


def sse(events):
    out = []
    for ev in events:
        out.append("event: %s\ndata: %s\n\n" % (ev["type"], json.dumps(ev)))
    return "".join(out).encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        body = b'{"object":"list","data":[]}'
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("content-length") or 0))
        body = json.loads(raw or b"{}")
        with open(LOG_FILE, "a") as f:
            f.write(json.dumps({"path": self.path, "body": body}) + "\n")
        rid = next_id("resp")
        step = plan(body)
        if step[0] == "call":
            item = {"type": "function_call", "call_id": next_id("call"), "name": step[1],
                    "arguments": json.dumps(step[2])}
        else:
            item = {"type": "message", "role": "assistant", "id": next_id("msg"),
                    "content": [{"type": "output_text", "text": step[1]}]}
        usage = {"input_tokens": 0, "input_tokens_details": None, "output_tokens": 0,
                 "output_tokens_details": None, "total_tokens": 0}
        out = sse([
            {"type": "response.created", "response": {"id": rid}},
            {"type": "response.output_item.done", "item": item},
            {"type": "response.completed", "response": {"id": rid, "usage": usage}},
        ])
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(PORT_FILE, "w") as f:
    f.write(str(server.server_address[1]))
server.serve_forever()
