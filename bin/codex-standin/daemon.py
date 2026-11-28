"""The stand-in app-server: a control socket under CODEX_HOME speaking
the JSON-RPC a Codex TUI and marspot speak to the real one, answering
in the shapes the recorded exchange shows.  It holds the threads, runs
their turns (cxthread.py) and holds each loaded thread's writer lock."""
import copy
import json
import os
import socket
import threading
import time

import cxthread as th
import toml_lite
import turns
import wsrpc

# a managed daemon nobody has talked to for this long goes away
IDLE_EXIT_S = 600
SANDBOX = {"danger-full-access": "dangerFullAccess", "workspace-write": "workspaceWrite", "read-only": "readOnly"}


class Daemon:
    def __init__(self, home, version, fixture, managed):
        self.home = os.path.realpath(home)
        self.version = version
        self.fixture = fixture
        self.managed = managed
        self.mu = threading.RLock()
        self.threads = {}
        self.conns = set()
        self.last_seen = time.time()

    # ── socket ────────────────────────────────────────────────────
    def serve(self):
        d = os.path.join(self.home, "app-server-control")
        os.makedirs(d, exist_ok=True)
        path = os.path.join(d, "app-server-control.sock")
        try:
            wsrpc.connect(path, 2).close()
            return  # another one answers there
        except OSError:
            pass
        if os.path.lexists(path):
            os.unlink(path)
        s = socket.socket(socket.AF_UNIX)
        wsrpc.at_short(path, s.bind)
        s.listen(16)
        if self.managed:
            self._pid_file()
        s.settimeout(30)
        while True:
            try:
                c, _ = s.accept()
            except socket.timeout:
                with self.mu:
                    idle = not self.conns and time.time() - self.last_seen > IDLE_EXIT_S
                if self.managed and idle:
                    return
                continue
            threading.Thread(target=self._conn, args=(c,), daemon=True).start()

    def _pid_file(self):
        d = os.path.join(self.home, "app-server-daemon")
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, "daemon.pid"), "w") as f:
            f.write('{"pid":%d,"version":"%s"}\n' % (os.getpid(), self.version))

    def _conn(self, sock):
        try:
            c = wsrpc.accept(sock)
        except (EOFError, OSError):
            return
        with self.mu:
            self.conns.add(c)
        try:
            while True:
                m = c.recv()
                if "id" in m and "method" in m:
                    self._answer(c, m)
        except (EOFError, OSError, ValueError):
            pass
        with self.mu:
            self.conns.discard(c)
            self.last_seen = time.time()
            for t in self.threads.values():
                t.subs.discard(c)
        c.close()

    def _answer(self, c, m):
        method, p = m["method"], m.get("params") or {}
        try:
            out = {"jsonrpc": "2.0", "id": m["id"], "result": self.handle(c, method, p)}
        except th.Refused as e:
            out = {"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32600, "message": str(e)}}
        c.send(out)

    # ── notifications ─────────────────────────────────────────────
    def broadcast(self, method, params, only=None):
        with self.mu:
            to = list(self.conns if only is None else only)
        for c in to:
            try:
                c.send({"jsonrpc": "2.0", "method": method, "params": params})
            except OSError:
                pass

    # ── methods ───────────────────────────────────────────────────
    def config(self, overrides=None):
        try:
            cfg = toml_lite.loads(open(os.path.join(self.home, "config.toml")).read())
        except FileNotFoundError:
            cfg = {}
        for k, v in (overrides or {}).items():
            toml_lite.set_path(cfg, k.split("."), v)
        return cfg

    def loaded(self, tid):
        with self.mu:
            t = self.threads.get(tid)
        if t is None:
            raise th.Refused("thread not loaded: %s" % tid)
        return t

    def handle(self, c, method, p):
        tid = p.get("threadId", "")
        if method == "initialize":
            return {"userAgent": "codex-tui/%s (stand-in)" % self.version, "codexHome": self.home}
        if method == "thread/start":
            return self.start(c, p)
        if method == "thread/resume":
            return self.resume(c, p)
        if method == "thread/read":
            return {"thread": self.loaded(tid).view(False)}
        if method == "thread/loaded/list":
            with self.mu:
                return {"data": list(self.threads), "nextCursor": None}
        if method == "thread/unsubscribe":
            return self.unsubscribe(c, tid)
        if method in ("turn/start", "thread/queue/add"):
            t = self.loaded(tid)
            texts = [i.get("text", "") for i in p.get("input") or [] if i.get("type") == "text"]
            t.enqueue(texts, p.get("clientUserMessageId"))
            return {} if method == "thread/queue/add" else {"turn": {"id": None, "status": "inProgress"}}
        if method == "turn/steer":
            return {"turnId": self.loaded(tid).steer(p)}
        if method == "turn/interrupt":
            return {}
        if method == "thread/inject_items":
            self.loaded(tid).inject(p.get("items") or [])
            return {}
        if method == "thread/goal/get":
            return {"goal": None}
        if method == "thread/goal/set":
            return {}
        if method == "thread/turns/list":
            t = self.loaded(tid)
            if not t.materialized():
                raise th.Refused("thread %s is not materialized yet; thread/turns/list is unavailable before first user message" % tid)
            return {"data": t.turns, "nextCursor": None}
        if method == "config/read":
            return {"config": self.config()}
        raise th.Refused("%s: not served by this app-server" % method)

    def _template(self, which):
        for probe in self.fixture.get("probes", []):
            r = probe.get(which, {}).get("result")
            if r:
                return copy.deepcopy(r)
        return {}

    def start(self, c, p):
        cfg = self.config(p.get("config"))
        if p.get("model"):
            cfg["model"] = p["model"]
        approval = p.get("approvalPolicy") or cfg.get("approval_policy") or "on-request"
        sandbox = p.get("sandbox") or cfg.get("sandbox_mode") or "read-only"
        t = th.Thread(self, turns.uuid7(), p.get("cwd") or os.getcwd(), cfg, approval, sandbox)
        t.lock()
        with self.mu:
            self.threads[t.id] = t
            t.subs.add(c)
        self.broadcast("thread/started", {"thread": t.view(False)})
        return {"thread": t.view(False)}

    def resume(self, c, p):
        tid = p.get("threadId", "")
        with self.mu:
            t = self.threads.get(tid)
        if t is None:
            t = th.Thread.from_rollout(self, tid)
            with self.mu:
                self.threads[tid] = t
            self.broadcast("thread/started", {"thread": t.view(False)})
        elif not t.materialized():
            # codex answers so for a loaded thread before its first user
            # message; marspot then classifies the pane from config/read
            raise th.Refused("no rollout found for thread id %s" % tid)
        with self.mu:
            t.subs.add(c)
        out = self._template("thread/resume")
        out.update({"thread": t.view(not p.get("excludeTurns")), "model": t.cfg.get("model"), "cwd": t.cwd,
                    "approvalPolicy": t.approval, "sandbox": {"type": SANDBOX.get(t.sandbox, "readOnly")}})
        return out

    def unsubscribe(self, c, tid):
        t = self.loaded(tid)
        with self.mu:
            t.subs.discard(c)
            gone = not t.subs and not t.running
            if gone:
                del self.threads[tid]
        if gone:
            t.unload()
            self.broadcast("thread/status/changed", {"threadId": tid, "status": {"type": "notLoaded"}})
        return {"status": "unsubscribed"}


def main(home, version, fixture_path, managed):
    fixture = json.load(open(fixture_path)) if fixture_path else {}
    Daemon(home, version, fixture, managed).serve()
