"""What the stand-in runs beside the model, read from its home as kimi
reads it: the MCP servers of `mcp.json` (stdio, started as direct
children, which is how marspot's server finds the session it serves)
and the `[[hooks]]` tables of `config.toml`, run through the shell
with the recorded stdin shapes."""
import json
import os
import select
import signal
import subprocess
import sys
import threading

# after this directory: the codex stand-in has modules of the same names
sys.path.append(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "codex-standin"))
import toml_lite  # noqa: E402

PROTOCOL = "2025-11-25"
# seconds a server has to answer the handshake
START_S = 15
HOOK_KEYS = {"event", "command", "timeout", "matcher"}


class Server:
    def __init__(self, name, spec, log):
        self.name, self.next_id = name, 0
        self.timeout = spec.get("toolTimeoutMs", 60000) / 1000
        env = dict(os.environ, **{k: str(v) for k, v in spec.get("env", {}).items()})
        self.p = subprocess.Popen([spec["command"], *spec.get("args", [])], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=log, env=env)
        self.tools, self.error = [], None

    def start(self):
        try:
            self.request("initialize", {"protocolVersion": PROTOCOL, "capabilities": {},
                                        "clientInfo": {"name": "kimi-code", "version": "0.0.0"}}, START_S)
            self.send({"method": "notifications/initialized", "jsonrpc": "2.0"})
            self.tools = self.request("tools/list", {}, START_S).get("tools", [])
        except (OSError, ValueError, TimeoutError, EOFError, RuntimeError) as e:
            self.error = e
            self.p.kill()
            self.p.wait()

    def send(self, msg):
        self.p.stdin.write((json.dumps(msg) + "\n").encode())
        self.p.stdin.flush()

    def request(self, method, params, timeout):
        rid, self.next_id = self.next_id, self.next_id + 1
        self.send({"method": method, "params": params, "jsonrpc": "2.0", "id": rid})
        fd = self.p.stdout.fileno()
        while True:
            if not select.select([fd], [], [], timeout)[0]:
                raise TimeoutError("%s: no answer to %s" % (self.name, method))
            line = self.p.stdout.readline()
            if not line:
                raise EOFError("%s: the server left" % self.name)
            msg = json.loads(line)
            if msg.get("id") == rid:
                if "error" in msg:
                    raise RuntimeError(msg["error"].get("message", "error"))
                return msg.get("result", {})

    def call(self, tool, args):
        r = self.request("tools/call", {"name": tool, "arguments": args}, self.timeout)
        return "\n".join(c.get("text", "") for c in r.get("content", []) if c.get("type") == "text")

    def close(self):
        self.p.stdin.close()
        try:
            self.p.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.p.kill()


def servers(home, log):
    try:
        table = json.load(open(os.path.join(home, "mcp.json"))).get("mcpServers", {})
    except (OSError, ValueError):
        return []
    started = []
    for name, spec in table.items():
        try:
            started.append(Server(name, spec, log))
        except (OSError, KeyError, TypeError) as e:
            log.write(("mcp %s: %r\n" % (name, e)).encode())
    # side by side: one server slow to answer does not hold up the others
    threads = [threading.Thread(target=s.start) for s in started]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    for s in started:
        if s.error:
            log.write(("mcp %s: %r\n" % (s.name, s.error)).encode())
    log.flush()
    return [s for s in started if not s.error]


def tool_specs(srvs):
    return [{"type": "function", "function": {"name": "mcp__%s__%s" % (s.name, t["name"]),
                                              "description": t.get("description", ""),
                                              "parameters": t.get("inputSchema", {"type": "object"})}}
            for s in srvs for t in s.tools]


def hooks(home):
    """The `[[hooks]]` tables of the home's config.toml, in order; raises
    ValueError for one with a key kimi does not accept."""
    try:
        text = open(os.path.join(home, "config.toml")).read()
    except FileNotFoundError:
        return []
    out, table = [], None
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if line == "[[hooks]]":
            table = {}
            out.append(table)
            continue
        if line.startswith("["):
            table = None
            continue
        if table is None:
            continue
        parts, i = toml_lite._key(line, 0)
        if parts[0] not in HOOK_KEYS:
            raise ValueError("hooks: unknown key %r" % parts[0])
        table[parts[0]] = toml_lite.value(line, line.index("=", i) + 1)[0]
    for t in out:
        if "event" not in t or "command" not in t:
            raise ValueError("hooks: a table without event or command")
    return out


def run_hooks(home, event, payload, cwd):
    """(status, stdout, stderr) of every hook for `event`."""
    got = []
    for h in hooks(home):
        if h["event"] != event:
            continue
        got.append(run_hook(h, payload, cwd))
    return got


def run_hook(h, payload, cwd):
    # its own group: on timeout the command the shell started goes too,
    # not only /bin/sh
    p = subprocess.Popen(["/bin/sh", "-c", h["command"]], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, text=True, cwd=cwd, start_new_session=True)
    try:
        out, err = p.communicate(json.dumps(payload, separators=(",", ":")), timeout=h.get("timeout", 60))
        return p.returncode, out, err
    except subprocess.TimeoutExpired:
        try:
            os.killpg(p.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        p.communicate()
        return None, "", ""
