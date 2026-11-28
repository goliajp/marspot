"""The stand-in TUI: a client of its home's app-server (started, as
Codex starts it, when nothing answers on the control socket), drawing
what the server reports about its thread, and sending what is typed."""
import glob
import json
import os
import queue
import select
import signal
import sys
import termios
import time
import tty

import screen as sc
import toml_lite
import wsrpc

HERE = os.path.dirname(os.path.abspath(__file__))


def parse(argv):
    o = {"config": {}, "approval": None, "sandbox": None, "model": None, "resume": None, "prompt": None}
    it = iter(argv)
    for a in it:
        flag, eq, inline = a.partition("=") if a.startswith("--") else (a, "", "")
        val = (lambda: inline) if eq else (lambda: next(it, ""))
        if flag in ("--yolo", "--dangerously-bypass-approvals-and-sandbox"):
            o["approval"], o["sandbox"] = "never", "danger-full-access"
        elif flag == "--full-auto":
            o["approval"], o["sandbox"] = "on-request", "workspace-write"
        elif flag in ("-a", "--ask-for-approval"):
            o["approval"] = val()
        elif flag in ("-s", "--sandbox"):
            o["sandbox"] = val()
        elif flag in ("-m", "--model"):
            o["model"] = val()
        elif flag in ("-c", "--config"):
            k, _, v = val().partition("=")
            try:
                o["config"][k.strip()] = toml_lite.value(v.strip())[0]
            except (ValueError, IndexError):
                o["config"][k.strip()] = v.strip()
        elif flag in ("-p", "--profile", "--add-dir", "-C", "--cd", "-i", "--image"):
            val()
        elif a == "resume" and o["resume"] is None and o["prompt"] is None:
            o["resume"] = next(it, "--last")
        elif not a.startswith("-") and o["prompt"] is None:
            o["prompt"] = a
    return o


def daemon_up(home, version):
    """Start the home's app-server when nothing answers, detached as
    Codex detaches its own, and wait for it to answer."""
    sock = os.path.join(home, "app-server-control", "app-server-control.sock")
    try:
        wsrpc.connect(sock, 2).close()
        return sock
    except OSError:
        pass
    logdir = os.path.join(home, "app-server-daemon")
    os.makedirs(logdir, exist_ok=True)
    pid = os.fork()
    if pid == 0:
        os.setsid()
        if os.fork():
            os._exit(0)
        null = os.open(os.devnull, os.O_RDWR)
        log = os.open(os.path.join(logdir, "daemon.stderr.log"), os.O_CREAT | os.O_WRONLY | os.O_APPEND, 0o600)
        os.dup2(null, 0)
        os.dup2(log, 1)
        os.dup2(log, 2)
        os.execv(sys.executable, [sys.executable, "-I", os.path.join(HERE, "standin.py"), "--daemon", home, version, "managed"])
    os.waitpid(pid, 0)
    for _ in range(200):
        try:
            wsrpc.connect(sock, 2).close()
            return sock
        except OSError:
            time.sleep(0.1)
    raise SystemExit("the app-server at %s did not answer" % sock)


def newest_for(home, cwd):
    best = None
    for p in glob.glob(os.path.join(home, "sessions", "*", "*", "*", "rollout-*.jsonl")):
        with open(p) as f:
            meta = json.loads(f.readline() or "{}").get("payload") or {}
        if meta.get("cwd") == cwd and (best is None or os.path.getmtime(p) > os.path.getmtime(best)):
            best = p
    return best and best[-42:-6]


class Tui:
    def __init__(self, argv, version, fixture):
        self.o = parse(argv)
        self.home = os.path.realpath(os.environ.get("CODEX_HOME") or os.path.expanduser("~/.codex"))
        self.cwd = os.getcwd()
        try:
            cfg = toml_lite.loads(open(os.path.join(self.home, "config.toml")).read())
        except FileNotFoundError:
            cfg = {}
        for k, v in self.o["config"].items():
            toml_lite.set_path(cfg, k.split("."), v)
        approval = self.o["approval"] or cfg.get("approval_policy")
        sandbox = self.o["sandbox"] or cfg.get("sandbox_mode")
        self.s = sc.Screen(fixture, version, self.cwd, self.o["model"] or cfg.get("model"),
                           approval == "never" and sandbox == "danger-full-access")
        self.version = version
        self.notes = queue.Queue()
        self.wake_r, self.wake_w = os.pipe()
        self.pending = ""
        self.paste = False
        self.c = None

    def out(self, s):
        os.write(1, s.encode())

    def wake(self, *_):
        os.write(self.wake_w, b"x")

    def run(self):
        sock = daemon_up(self.home, self.version)
        saved = termios.tcgetattr(0)
        tty.setraw(0)
        for sig in (signal.SIGTERM, signal.SIGHUP):
            signal.signal(sig, self.stop)
        signal.signal(signal.SIGWINCH, self.wake)
        self.out(self.s.setup())
        code = 0
        try:
            self.c = wsrpc.Client(sock, "codex-tui", on_note=lambda m: (self.notes.put(m), self.wake()))
            self.open_thread()
            self.out(self.s.frame())
            code = self.loop()
        except SystemExit as e:
            code = e.code or 0
        finally:
            self.out(self.s.teardown(code == 0))
            # not a drain: a terminal nobody reads any more would hold the exit
            termios.tcsetattr(0, termios.TCSANOW, saved)
        return code

    def stop(self, *_):
        raise SystemExit(143)

    def open_thread(self):
        o = self.o
        if o["resume"]:
            tid = newest_for(self.home, self.cwd) if o["resume"] == "--last" else o["resume"]
            r = self.c.call("thread/resume", {"threadId": tid or ""}) if tid else {"error": {"message": "no session to resume here"}}
        else:
            r = self.c.call("thread/start", {"cwd": self.cwd, "config": o["config"], "model": o["model"],
                                             "approvalPolicy": o["approval"], "sandbox": o["sandbox"]})
        if "error" in r:
            self.s.add("error", r["error"].get("message", ""))
            return
        t = r["result"]["thread"]
        self.s.thread = t["id"]
        self.s.name = t.get("name")
        for turn in t.get("turns") or []:
            for item in turn.get("items") or []:
                self.show(item)
        self.out(self.s.state("idle"))
        if o["prompt"]:
            self.submit(o["prompt"])

    def submit(self, text):
        if not self.s.thread:
            return
        if self.s.name is None:
            self.s.name = text.strip().split("\n")[0][:50]
        self.c.call("turn/start", {"threadId": self.s.thread, "input": [{"type": "text", "text": text}]})

    def show(self, item):
        k = item.get("type")
        if k == "userMessage":
            self.s.add("user", "".join(c.get("text", "") for c in item.get("content") or []))
        elif k == "agentMessage":
            self.s.add("agent", item.get("text", ""))
        elif k == "mcpToolCall":
            self.s.add("tool", "Called %s.%s(%s)" % (item.get("server"), item.get("tool"), json.dumps(item.get("arguments"))))

    def note(self, m):
        if m is None:
            raise SystemExit(1)
        p = m.get("params") or {}
        if p.get("threadId") != self.s.thread:
            return
        method = m.get("method")
        if method == "item/completed":
            self.show(p.get("item") or {})
        elif method == "turn/started":
            self.s.working = True
            self.out(self.s.state("working"))
        elif method == "turn/completed":
            self.s.working = False
            self.s.add("worked", sc.worked_line())
            self.out(self.s.state("done"))

    def loop(self):
        while True:
            r = select.select([0, self.wake_r], [], [])[0]
            if self.wake_r in r:
                os.read(self.wake_r, 4096)
                while not self.notes.empty():
                    self.note(self.notes.get())
            if 0 in r:
                data = os.read(0, 4096)
                if not data:
                    return 0
                done = self.keys(data.decode("utf-8", "replace"))
                if done is not None:
                    return done
            self.out(self.s.frame())

    def keys(self, data):
        """Edits the draft; an exit code once the TUI is to quit."""
        s, i = self.pending + data, 0
        self.pending = ""
        while i < len(s):
            ch = s[i]
            if ch == "\x1b":
                end = seq_end(s, i)
                if end is None:
                    self.pending = s[i:]
                    break
                seq, i = s[i:end], end
                if seq in ("\x1b[13u", "\x1b[13;1u"):
                    ch = "\r"
                elif seq == "\x1b[200~" or seq == "\x1b[201~":
                    self.paste = seq == "\x1b[200~"
                    continue
                else:
                    continue
            else:
                i += 1
            if ch in "\r\n" and self.paste:
                self.s.draft += " "
            elif ch in "\r\n":
                text, self.s.draft = self.s.draft.strip(), ""
                if text in ("/quit", "/exit"):
                    return 0
                if text:
                    self.submit(text)
            elif ch in "\x7f\x08":
                self.s.draft = self.s.draft[:-1]
            elif ch in "\x03\x04":
                if not self.s.draft:
                    return 0
                self.s.draft = ""
            elif ch == "\x15":
                self.s.draft = ""
            elif ch >= " ":
                self.s.draft += ch
        return None


def seq_end(s, i):
    """Where the escape sequence at s[i] ends; None when it is cut off."""
    if i + 1 >= len(s):
        return None
    k = s[i + 1]
    if k == "[":
        j = i + 2
        while j < len(s) and not ("\x40" <= s[j] <= "\x7e"):
            j += 1
        return j + 1 if j < len(s) else None
    if k in "]P_^":
        for j in range(i + 2, len(s)):
            if s[j] == "\x07":
                return j + 1
            if s[j] == "\x1b" and j + 1 < len(s) and s[j + 1] == "\\":
                return j + 2
        return None
    return i + 2
