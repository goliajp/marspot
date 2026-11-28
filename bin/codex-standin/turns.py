"""What a stand-in thread does with its input: the model behind it (any
Responses endpoint, here the test's model stand-in), the MCP servers of
its config, and the rollout file Codex keeps for it."""
import glob
import json
import os
import select
import subprocess
import time
import urllib.request
import uuid

MAX_STEPS = 8


def uuid7():
    ms = int(time.time() * 1000)
    r = uuid.uuid4().int
    v = (ms << 80) | (0x7 << 76) | (r & ((1 << 76) - 1))
    v = (v & ~(0x3 << 62)) | (0x2 << 62)
    return str(uuid.UUID(int=v))


def iso(t=None):
    t = time.time() if t is None else t
    return time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(t)) + ".%03dZ" % int((t % 1) * 1000)


class Mcp:
    """One MCP server over stdio, started the way the config says."""

    def __init__(self, name, spec, cwd, version):
        env = dict(os.environ)
        env.update({k: str(v) for k, v in (spec.get("env") or {}).items()})
        self.name = name
        self.p = subprocess.Popen([spec["command"]] + list(spec.get("args") or []), cwd=cwd, env=env,
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.n = 0
        self.buf = b""
        self.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                    "clientInfo": {"name": "codex-mcp-client", "version": version}})
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        self.tools = self.request("tools/list", {}).get("result", {}).get("tools", [])

    def _send(self, obj):
        self.p.stdin.write((json.dumps(obj) + "\n").encode())
        self.p.stdin.flush()

    def request(self, method, params, timeout=30):
        self.n += 1
        n = self.n
        self._send({"jsonrpc": "2.0", "id": n, "method": method, "params": params})
        end = time.time() + timeout
        fd = self.p.stdout.fileno()
        while time.time() < end:
            while b"\n" in self.buf:
                line, self.buf = self.buf.split(b"\n", 1)
                m = json.loads(line or b"{}")
                if m.get("id") == n:
                    return m
            if select.select([fd], [], [], max(0.0, end - time.time()))[0]:
                got = os.read(fd, 65536)
                if not got:
                    break
                self.buf += got
        return {"error": {"message": "%s: no answer from the %s server" % (method, self.name)}}

    def close(self):
        self.p.stdin.close()
        self.p.wait(timeout=5)


def tools_for(mcps):
    out = []
    for m in mcps:
        out.append({"type": "namespace", "name": "mcp__" + m.name, "tools": [
            {"type": "function", "name": t["name"], "description": t.get("description", ""),
             "parameters": t.get("inputSchema", {})} for t in m.tools]})
    return out


def model_step(cfg, body):
    """The items of one model response, from its server-sent events."""
    provider = (cfg.get("model_providers") or {}).get(cfg.get("model_provider", ""), {})
    base = provider.get("base_url", "")
    req = urllib.request.Request(base.rstrip("/") + "/responses", data=json.dumps(body).encode(),
                                 headers={"content-type": "application/json"})
    items = []
    with urllib.request.urlopen(req, timeout=60) as r:
        for line in r.read().decode().splitlines():
            if line.startswith("data: "):
                ev = json.loads(line[6:])
                if ev.get("type") == "response.output_item.done":
                    items.append(ev["item"])
    return items


class Rollout:
    """The thread's rollout file, written as Codex writes its own: a
    session_meta line, then one line per thing that happened."""

    def __init__(self, home, thread, cwd, version, path=None):
        t = time.time()
        if path is None:
            day = time.strftime("%Y/%m/%d", time.localtime(t))
            stamp = time.strftime("%Y-%m-%dT%H-%M-%S", time.localtime(t))
            path = os.path.join(home, "sessions", day, "rollout-%s-%s.jsonl" % (stamp, thread))
        self.path = path
        self.meta = {"session_id": thread, "id": thread, "timestamp": iso(t), "cwd": cwd, "originator": "codex-tui",
                     "cli_version": version, "source": "vscode", "thread_source": "user", "model_provider": "mock"}
        self.n = 0

    def write(self, kind, payload):
        new = not os.path.exists(self.path)
        os.makedirs(os.path.dirname(self.path), exist_ok=True)
        with open(self.path, "a") as f:
            if new:
                f.write(json.dumps({"timestamp": self.meta["timestamp"], "type": "session_meta", "payload": self.meta}) + "\n")
            self.n += 1
            f.write(json.dumps({"timestamp": iso(), "ordinal": self.n, "type": kind, "payload": payload}) + "\n")

    def message(self, role, text, turn):
        kind = "input_text" if role == "user" else "output_text"
        self.write("response_item", {"type": "message", "role": role, "content": [{"type": kind, "text": text}],
                                     "internal_chat_message_metadata_passthrough": {"turn_id": turn}})


def find_rollout(home, thread):
    hits = glob.glob(os.path.join(home, "sessions", "*", "*", "*", "rollout-*-%s.jsonl" % thread))
    return hits[0] if hits else None


def read_rollout(path):
    """(session meta, turns) of a rollout: each turn its user and agent
    messages, as items."""
    meta, turns = None, []
    for line in open(path):
        d = json.loads(line)
        p = d.get("payload") or {}
        if d.get("type") == "session_meta":
            meta = p
        elif d.get("type") == "event_msg" and p.get("type") == "task_started":
            turns.append({"id": p.get("turn_id"), "status": "completed", "items": []})
        elif d.get("type") == "response_item" and p.get("type") == "message" and turns:
            text = "".join(c.get("text", "") for c in p.get("content") or [])
            if p.get("role") == "user":
                turns[-1]["items"].append({"type": "userMessage", "id": uuid7(), "content": [{"type": "text", "text": text}]})
            elif p.get("role") == "assistant":
                turns[-1]["items"].append({"type": "agentMessage", "id": uuid7(), "text": text, "phase": "final_answer"})
    return meta, turns
