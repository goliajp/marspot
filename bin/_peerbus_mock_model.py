#!/usr/bin/env python3
"""A stand-in for the model behind Codex, for the peerbus end-to-end test.

Speaks just enough of the Responses API (POST <base>/responses, answered
as server-sent events) for a Codex TUI to run a turn without an account.
It messages peers only through the marspot MCP tools (the `mcp__marspot`
namespace), the way a real model would:

- a <cross-session-message from="ADDR"> whose body asks for a reply is
  answered with SendMessage(to=ADDR, message=REPLY_TEXT);
- a body saying `Send NAME the text TEXT` becomes SendMessage(to=NAME,
  message=TEXT);
- once a tool has run, the turn ends with a short message.

Anything else is answered with a short message.  Without the marspot
tools on offer the turn says so, and the test sees no reply.

Usage: _peerbus_mock_model.py <port-file> <log-file>
Binds an ephemeral port on 127.0.0.1 and writes it to <port-file>.
Every request body is appended to <log-file>, one JSON line each.
"""
import json
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REPLY_TEXT = "PEERBUS-E2E-REPLY"
ENVELOPE = re.compile(r'<cross-session-message from="(uds:[^"]+)"')
FORWARD = re.compile(r"Send (\S+) the text (\S+)")
NAMESPACE = "mcp__marspot"

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


def has_marspot_tools(tools):
    for t in tools:
        if isinstance(t, dict) and t.get("name") == NAMESPACE:
            names = {x.get("name") for x in t.get("tools") or []}
            return {"ListAgents", "SendMessage"} <= names
    return False


def plan(body):
    items = [i for i in body.get("input") or [] if isinstance(i, dict)]
    last = items[-1] if items else {}
    if last.get("type") == "function_call_output":
        return ("message", "sent")
    if last.get("type") != "message" or last.get("role") != "user":
        return ("message", "ok")
    text = text_of(last)
    if not has_marspot_tools(body.get("tools") or []):
        return ("message", "no marspot tools on offer")
    fwd = FORWARD.search(text)
    if fwd:
        return ("call", {"to": fwd.group(1), "message": fwd.group(2)})
    env = ENVELOPE.search(text)
    if env and "Reply to this message" in text:
        return ("call", {"to": env.group(1), "message": REPLY_TEXT})
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
            item = {"type": "function_call", "call_id": next_id("call"), "namespace": NAMESPACE,
                    "name": "SendMessage", "arguments": json.dumps(step[1])}
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
