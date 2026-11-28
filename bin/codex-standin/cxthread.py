"""One stand-in thread: its history, the clients subscribed to it, its
writer lock, and a worker that runs its queued input as turns."""
import copy
import fcntl
import json
import os
import threading
import time

import turns


class Refused(Exception):
    """Answered to the client as a JSON-RPC error."""


class Thread:
    def __init__(self, daemon, tid, cwd, cfg, approval, sandbox, rollout_path=None):
        self.d = daemon
        self.id = tid
        self.cwd = cwd
        self.cfg = cfg
        self.approval = approval
        self.sandbox = sandbox
        self.created = int(time.time())
        self.status = "idle"
        self.turns = []
        self.subs = set()
        self.queue = []
        self.steering = []
        self.running = False
        self.active = None
        self.name = None
        self.mcps = None
        self.lockfd = None
        self.rollout = turns.Rollout(daemon.home, tid, cwd, daemon.version, rollout_path)

    @classmethod
    def from_rollout(cls, daemon, tid):
        path = turns.find_rollout(daemon.home, tid)
        if path is None:
            raise Refused("no rollout found for thread id %s" % tid)
        meta, past = turns.read_rollout(path)
        cfg = daemon.config()
        t = cls(daemon, tid, (meta or {}).get("cwd") or os.getcwd(), cfg,
                cfg.get("approval_policy") or "on-request", cfg.get("sandbox_mode") or "read-only", path)
        t.lock()
        t.turns = past
        return t

    def lock(self):
        d = os.path.join(self.d.home, "thread-writer-locks")
        os.makedirs(d, exist_ok=True)
        fd = os.open(os.path.join(d, self.id + ".lock"), os.O_CREAT | os.O_RDWR, 0o600)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            os.close(fd)
            raise Refused("thread %s already has an active writer" % self.id)
        self.lockfd = fd

    def unload(self):
        if self.lockfd is not None:
            os.close(self.lockfd)
            self.lockfd = None
        for m in self.mcps or []:
            m.close()
        self.mcps = None

    def materialized(self):
        """Codex writes the rollout with the thread's first user message."""
        return os.path.exists(self.rollout.path)

    def view(self, with_turns):
        v = self.d._template("thread/read").get("thread", {})
        preview = next((i["content"][0]["text"] for t in self.turns for i in t["items"] if i["type"] == "userMessage"), "")
        v.update({"id": self.id, "sessionId": self.id, "cwd": self.cwd, "preview": preview, "name": self.name,
                  "environments": [{"environmentId": "local", "cwd": self.cwd, "runtimeWorkspaceRoots": [self.cwd]}],
                  "model": self.cfg.get("model"), "modelProvider": self.cfg.get("model_provider"),
                  "createdAt": self.created, "updatedAt": int(time.time()), "recencyAt": int(time.time()),
                  "status": self.status_value(), "path": self.rollout.path, "cliVersion": self.d.version,
                  "parentThreadId": None, "ephemeral": False, "threadSource": "user",
                  "turns": copy.deepcopy(self.turns) if with_turns else []})
        return v

    def status_value(self):
        return {"type": "active", "activeFlags": []} if self.status == "active" else {"type": self.status}

    # ── input ─────────────────────────────────────────────────────
    def enqueue(self, texts, client_id):
        with self.d.mu:
            self.queue.append((texts, client_id))
            if self.running:
                return
            self.running = True
        threading.Thread(target=self._work, daemon=True).start()

    def steer(self, p):
        with self.d.mu:
            if not self.active or self.active != p.get("expectedTurnId", self.active):
                raise Refused("no active turn to steer")
            self.steering.extend(i.get("text", "") for i in p.get("input") or [] if i.get("type") == "text")
            return self.active

    def inject(self, items):
        for i in items:
            text = "".join(c.get("text", "") for c in i.get("content") or [])
            self.rollout.message(i.get("role", "user"), text, None)

    # ── turns ─────────────────────────────────────────────────────
    def _work(self):
        while True:
            with self.d.mu:
                if not self.queue:
                    self.running = False
                    return
                texts, client_id = self.queue.pop(0)
            self._turn(texts, client_id)

    def _note(self, method, params, everyone=False):
        params = dict(params, threadId=self.id)
        with self.d.mu:
            subs = set(self.subs)
        self.d.broadcast(method, params, None if everyone else subs)

    def _set_status(self, status):
        self.status = status
        self._note("thread/status/changed", {"status": self.status_value()}, everyone=True)

    def _item(self, turn, item):
        turn["items"].append(item)
        self._note("item/completed", {"turnId": turn["id"], "item": item})

    def _user(self, turn, text, client_id, inputs):
        item = {"type": "userMessage", "id": turns.uuid7(), "content": [{"type": "text", "text": text}]}
        if client_id:
            item["clientId"] = client_id
        self._item(turn, item)
        self.rollout.message("user", text, turn["id"])
        inputs.append({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})

    def _turn(self, texts, client_id):
        turn = {"id": turns.uuid7(), "status": "inProgress", "items": []}
        with self.d.mu:
            self.turns.append(turn)
            self.active = turn["id"]
        self.rollout.write("event_msg", {"type": "task_started", "turn_id": turn["id"]})
        self.rollout.write("turn_context", {"turn_id": turn["id"], "cwd": self.cwd, "approval_policy": self.approval,
                                            "model": self.cfg.get("model"), "sandbox_policy": {"type": self.sandbox}})
        self._set_status("active")
        self._note("turn/started", {"turn": {"id": turn["id"], "status": "inProgress", "items": []}})
        inputs = self._history()
        for text in texts:
            self._user(turn, text, client_id, inputs)
        last = self._run(turn, inputs)
        self.rollout.write("event_msg", {"type": "task_complete", "turn_id": turn["id"], "last_agent_message": last})
        with self.d.mu:
            turn["status"] = "completed"
            self.active = None
        self._note("turn/completed", {"turn": {"id": turn["id"], "status": "completed", "items": [], "error": None}})
        self._set_status("idle")

    def _history(self):
        out = []
        for t in self.turns[:-1]:
            for i in t["items"]:
                if i["type"] == "userMessage":
                    out.append({"type": "message", "role": "user", "content": [{"type": "input_text", "text": i["content"][0]["text"]}]})
                elif i["type"] == "agentMessage":
                    out.append({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": i["text"]}]})
        return out

    def _run(self, turn, inputs):
        """The model loop of one turn; the last thing the agent said."""
        if self.mcps is None:
            servers = self.cfg.get("mcp_servers") or {}
            self.mcps = [turns.Mcp(n, s, self.cwd, self.d.version) for n, s in servers.items() if s.get("command")]
        tools = turns.tools_for(self.mcps)
        last = ""
        for _ in range(turns.MAX_STEPS):
            with self.d.mu:
                steer, self.steering = self.steering, []
            for text in steer:
                self._user(turn, text, None, inputs)
            try:
                items = turns.model_step(self.cfg, {"model": self.cfg.get("model"), "input": inputs, "tools": tools, "stream": True})
            except OSError as e:
                items = [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "stream error: %s" % e}]}]
            called = False
            for it in items:
                if it.get("type") == "function_call":
                    called = True
                    inputs += [it, self._call(turn, it)]
                elif it.get("type") == "message":
                    last = "".join(c.get("text", "") for c in it.get("content") or [])
                    self._item(turn, {"type": "agentMessage", "id": it.get("id") or turns.uuid7(), "text": last, "phase": "final_answer"})
                    self.rollout.message("assistant", last, turn["id"])
            if not called:
                return last
        return last

    def _call(self, turn, it):
        server = (it.get("namespace") or "").replace("mcp__", "", 1)
        args = json.loads(it.get("arguments") or "{}")
        m = next((m for m in self.mcps if m.name == server), None)
        if m is None:
            r = {"error": {"message": "no MCP server %s" % server}}
        else:
            r = m.request("tools/call", {"name": it["name"], "arguments": args, "_meta": {"threadId": self.id}})
        result = r.get("result") or {"isError": True, "content": [{"type": "text", "text": r.get("error", {}).get("message", "")}]}
        self._item(turn, {"type": "mcpToolCall", "id": it.get("call_id"), "server": server, "tool": it["name"],
                          "arguments": args, "status": "completed", "result": result})
        text = "".join(c.get("text", "") for c in result.get("content") or [])
        return {"type": "function_call_output", "call_id": it.get("call_id"), "output": text}
