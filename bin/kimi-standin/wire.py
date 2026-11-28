"""A session as the recorded kimi keeps one under its home: the
directory `sessions/<workspace>/<session id>`, its line in
`session_index.jsonl`, the lock it holds while a turn runs, and the
record `agents/main/wire.jsonl`, whose lines are the recorded ones with
this session's values put in."""
import copy
import hashlib
import json
import os
import re
import time
import uuid

ENV_ALIAS = "__kimi_env_model__"


def now_ms():
    return int(time.time() * 1000)


class Templates:
    """The first recorded record of each type; a loop event is keyed
    `context.append_loop_event:<event type>`."""

    def __init__(self, fixture):
        self.by = {}
        for line in fixture["wire"].splitlines():
            v = json.loads(line)
            key = v.get("type", "")
            if key == "context.append_loop_event":
                key += ":" + v["event"]["type"]
            self.by.setdefault(key, v)
        # the pid is a bare number in the lock, so its placeholder is too
        self.lock = json.loads(next(iter(fixture["locks"].values())).replace("{PID}", "0"))

    def make(self, key, **fields):
        v = copy.deepcopy(self.by[key])
        if "event" in v:
            v["event"].update(fields.pop("event", {}))
        v.update(fields)
        if "time" in v:
            v["time"] = now_ms()
        return v


def workspace_of(cwd):
    name = re.sub(r"[^A-Za-z0-9]", "", os.path.basename(cwd)) or "root"
    return "wd_%s_%s" % (name, hashlib.sha256(cwd.encode()).hexdigest()[:12])


def index_lines(home):
    try:
        lines = open(os.path.join(home, "session_index.jsonl")).read().splitlines()
    except OSError:
        return []
    out = []
    for line in lines:
        try:
            out.append(json.loads(line))
        except ValueError:
            pass
    return out


def find(home, cwd, session_id=None):
    """The directory of `session_id`, or of the newest session in `cwd`."""
    hits = [v for v in index_lines(home) if v.get("sessionId") == session_id or
            (session_id is None and v.get("workDir") == cwd)]
    return hits[-1]["sessionDir"] if hits else None


class Session:
    def __init__(self, tpl, home, cwd, directory=None):
        self.tpl, self.home, self.cwd = tpl, home, cwd
        if directory is None:
            self.id = "session_%s" % uuid.uuid4()
            self.dir = os.path.join(home, "sessions", workspace_of(cwd), self.id)
            self.fresh = True
        else:
            self.dir, self.id, self.fresh = directory, os.path.basename(directory), False
        self.path = os.path.join(self.dir, "agents", "main", "wire.jsonl")

    def open(self, alias, effort, yolo, servers):
        """Create the session, or reopen it, writing what the recorded
        kimi writes when it binds one."""
        os.makedirs(os.path.dirname(self.path), exist_ok=True)
        if self.fresh:
            line = {"sessionId": self.id, "sessionDir": self.dir, "workDir": self.cwd}
            with open(os.path.join(self.home, "session_index.jsonl"), "a") as f:
                f.write(json.dumps(line, separators=(",", ":")) + "\n")
            meta = self.tpl.make("metadata")
            meta["created_at"] = now_ms()
            self.write(meta)
        self.write(self.tpl.make("environment.set_binding"))
        for name, tools in servers:
            digest = hashlib.sha256(json.dumps(tools).encode()).hexdigest()
            self.write(self.tpl.make("mcp.tools_discovered", serverName=name, hash=digest, tools=tools,
                                     enabledNames=[t["name"] for t in tools]))
        self.write(self.tpl.make("changeNotifier.snapshot",
                                 workspacePrimaryByWorkspace={workspace_of(self.cwd): self.cwd}))
        self.write(self.tpl.make("profile.bind", modelAlias=alias, thinkingEffort=effort))
        if yolo:
            self.write(self.tpl.make("permission.set_mode", mode="yolo"))

    def write(self, record):
        text = json.dumps(record, separators=(",", ":"), ensure_ascii=False)
        # the recorded values that are this session's own
        for name, value in (("{KIMI_HOME}", self.home), ("{CWD}", self.cwd), ("{SESSION}", self.id),
                            ("{WORKSPACE}", workspace_of(self.cwd)), ("{HOME}", os.path.expanduser("~"))):
            text = text.replace(name, json.dumps(value)[1:-1])
        with open(self.path, "a") as f:
            f.write(text + "\n")

    def lock_path(self):
        return os.path.join(self.home, "locks", "sessions", self.id + ".lock")

    def lock(self, pid):
        v = dict(self.tpl.lock, pid=pid, token="%d:%s" % (pid, uuid.uuid4()), hostname=os.uname().nodename,
                 startedAt=now_ms())
        os.makedirs(os.path.dirname(self.lock_path()), exist_ok=True)
        with open(self.lock_path(), "w") as f:
            f.write(json.dumps(v, separators=(",", ":")))

    def unlock(self):
        try:
            os.remove(self.lock_path())
        except FileNotFoundError:
            pass

    def history(self):
        """The conversation so far, as model messages, and the next turn
        number: what a resumed session goes on from."""
        messages, turns = [], 0
        try:
            lines = open(self.path).read().splitlines()
        except OSError:
            return messages, turns
        for line in lines:
            v = json.loads(line)
            if v.get("type") == "turn.prompt":
                turns = v.get("turnId", turns) + 1
                text = "\n".join(p.get("text", "") for p in v.get("input", []))
                messages.append({"role": "user", "content": text})
            elif v.get("type") == "context.append_loop_event" and v["event"].get("type") == "content.part":
                if messages and messages[-1]["role"] == "assistant":
                    messages[-1]["content"] += v["event"]["part"].get("text", "")
                else:
                    messages.append({"role": "assistant", "content": v["event"]["part"].get("text", "")})
        return messages, turns
