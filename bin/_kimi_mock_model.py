#!/usr/bin/env python3
"""A stand-in for the model behind Kimi Code, for the kimi end-to-end test.

Speaks just enough of the chat-completions API (POST <base>/chat/completions,
answered as server-sent events) for a kimi TUI to run a turn without an
account.  What it does is decided by what was said since the model last
spoke, so one message is answered once:

- a <cross-session-message from="ADDR"> whose body asks for a reply is
  answered with the marspot SendMessage tool (to=ADDR, message=REPLY_TEXT);
- a body saying `Send NAME the text TEXT` becomes SendMessage(to=NAME,
  message=TEXT);
- `SLOW` anywhere in it holds the answer back for SLOW_SECONDS, so a turn
  can be seen running;
- a line starting with `QUOTA-EXHAUSTED` is refused the way the service
  refuses an account that has used its quota up: HTTP 429 with
  `exceeded_current_quota_error`.  Quoted in a handed-over conversation
  the words are not at the start of a line, and are answered normally;
- once a tool has run, and for anything else, the turn ends with a short
  message.

It also stands in for the account service: GET <base>/usages and
GET <base>/me are answered with the file <accounts-dir>/<token>.usages.json
or <token>.me.json, as written by the test, for the bearer token the
request carries; 401 when there is no such file.

Usage: _kimi_mock_model.py <port-file> <log-file> [<accounts-dir>]
Binds an ephemeral port on 127.0.0.1 and writes it to <port-file>.
Every request body is appended to <log-file>, one JSON line each.
"""
import json
import os
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REPLY_TEXT = "KIMI-E2E-REPLY"
SLOW_SECONDS = 8
ENVELOPE = re.compile(r'<cross-session-message from="(uds:[^"]+)"')
FORWARD = re.compile(r"Send (\S+) the text (\S+)")
TOKEN = re.compile(r"Bearer ([A-Za-z0-9-]+)$")
port_file, log_file = sys.argv[1], sys.argv[2]
accounts_dir = sys.argv[3] if len(sys.argv) > 3 else None


def text_of(message):
    content = message.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(p.get("text", "") for p in content if isinstance(p, dict))
    return ""


def send_tool(body):
    for t in body.get("tools", []):
        name = t.get("function", {}).get("name", "")
        if name.startswith("mcp__marspot") and name.endswith("SendMessage"):
            return name
    return None


def asked(body):
    """What the user side said since the model last spoke.  kimi appends
    reminders of its own as further user messages, so the prompt is not
    always the last one."""
    said = []
    for m in reversed(body.get("messages", [])):
        if m.get("role") != "user":
            break
        said.append(text_of(m))
    return "\n".join(reversed(said))


def decide(body):
    """What to answer: ("quota",), ("tool", name, args), or ("say", text)."""
    messages = body.get("messages", [])
    if not messages or messages[-1].get("role") == "tool":
        return ("say", "done")
    last_user = asked(body)
    if any(line.startswith("QUOTA-EXHAUSTED") for line in last_user.splitlines()):
        return ("quota",)
    tool = send_tool(body)
    envelope = ENVELOPE.search(last_user)
    forward = FORWARD.search(last_user)
    if tool and envelope and "Reply to this message" in last_user:
        return ("tool", tool, {"to": envelope.group(1), "message": REPLY_TEXT})
    if tool and forward:
        return ("tool", tool, {"to": forward.group(1), "message": forward.group(2)})
    return ("say", "mock reply done")


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply_json(self, status, obj):
        data = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        what = self.path.split("?")[0].rsplit("/", 1)[-1]
        if accounts_dir and what in ("usages", "me"):
            token = TOKEN.match(self.headers.get("Authorization", ""))
            path = os.path.join(accounts_dir, "%s.%s.json" % (token.group(1), what)) if token else ""
            if not os.path.isfile(path):
                return self.reply_json(401, {"error": {"message": "unauthorized"}})
            with open(path) as f:
                return self.reply_json(200, json.load(f))
        self.reply_json(200, {"object": "list", "data": [{"id": "mock-model", "object": "model"}]})

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        try:
            body = json.loads(raw)
        except ValueError:
            body = {}
        with open(log_file, "a") as log:
            log.write(json.dumps(body) + "\n")
        what = decide(body)
        if "SLOW" in asked(body) and what[0] == "say":
            time.sleep(SLOW_SECONDS)
        if what[0] == "quota":
            error = {"code": "exceeded_current_quota_error", "type": "exceeded_current_quota_error",
                     "message": "You have exceeded your current quota."}
            return self.reply_json(429, {"error": error})
        base = {"id": "chatcmpl-mock", "object": "chat.completion.chunk", "created": int(time.time()), "model": "mock-model"}
        usage = {"prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13}
        if not body.get("stream"):
            message = {"role": "assistant", "content": what[1] if what[0] == "say" else ""}
            done = {"index": 0, "message": message, "finish_reason": "stop"}
            return self.reply_json(200, {**base, "object": "chat.completion", "choices": [done], "usage": usage})
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()

        def event(delta, finish=None, with_usage=False):
            chunk = {**base, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
            if with_usage:
                chunk["usage"] = usage
            self.wfile.write(b"data: " + json.dumps(chunk).encode() + b"\n\n")
            self.wfile.flush()

        event({"role": "assistant", "content": ""})
        if what[0] == "tool":
            call = {"index": 0, "id": "call_%d" % int(time.time() * 1000), "type": "function",
                    "function": {"name": what[1], "arguments": json.dumps(what[2])}}
            event({"tool_calls": [call]})
            event({}, "tool_calls", True)
        else:
            event({"content": what[1]})
            event({}, "stop", True)
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(port_file, "w") as f:
    f.write(str(server.server_address[1]))
server.serve_forever()
