"""The stand-in TUI: draws what screen.py lays out, reads what is typed
or pasted, and runs each submitted prompt as a turn (turn.py) on a
thread of its own, so that the screen and the keys stay live while the
model is asked."""
import codecs
import fcntl
import os
import queue
import select
import signal
import struct
import sys
import termios
import threading
import tty

import peers
import screen as sc
import wire
from turn import Turn


class App:
    def __init__(self, opts, fixture):
        self.opts, self.fixture = opts, fixture
        self.home = os.environ.get("KIMI_CODE_HOME") or os.path.expanduser("~/.kimi-code")
        self.cwd = os.getcwd()
        # the compiled `kimi` in front of this process is the one
        # marspot sees, so the lock names it
        self.pid = os.getppid()
        self.tpl = wire.Templates(fixture)
        env_model = os.environ.get("KIMI_MODEL_NAME")
        self.alias = wire.ENV_ALIAS if env_model else (opts["model"] or "kimi-code/kimi-for-coding")
        self.model = env_model or self.alias.split("/")[-1]
        self.session, self.messages, self.turns, self.title = None, [], 0, None
        self.events, self.queued, self.busy = queue.Queue(), [], False
        self.wake_r, self.wake_w = os.pipe()
        # where kimi keeps its own log; the MCP servers' stderr goes there too
        os.makedirs(os.path.join(self.home, "logs"), exist_ok=True)
        self.log = open(os.path.join(self.home, "logs", "kimi-code.log"), "ab")
        self.servers, self.tool_specs = [], []
        self.live, self.status, self.typed = sc.Live(), sc.status_idle(), ""

    def post(self, kind, value):
        self.events.put((kind, value))
        os.write(self.wake_w, b".")

    def cols(self):
        try:
            return struct.unpack("HHHH", fcntl.ioctl(1, termios.TIOCGWINSZ, b"\0" * 8))[1] or 120
        except OSError:
            return 120

    def out(self, s):
        os.write(1, s.encode())

    def draw(self, above=()):
        self.out(self.live.frame(self.cols(), self.status, self.typed, self.model, self.cwd, above))

    def open_session(self):
        resume = self.opts["resume"]
        if resume is not None:
            found = wire.find(self.home, self.cwd, None if resume == "" else resume)
            if found:
                self.session = wire.Session(self.tpl, self.home, self.cwd, found)
                self.messages, self.turns = self.session.history()
                self.bind()

    def bind(self):
        s = self.session
        s.alias = self.alias
        s.open(self.alias, "on", self.opts["yolo"], [(x.name, x.tools) for x in self.servers])

    def submit(self, text):
        if text in ("/exit", "/quit"):
            raise SystemExit(0)
        if self.busy:
            self.queued.append(text)
            return
        if self.session is None:
            self.session = wire.Session(self.tpl, self.home, self.cwd)
            self.bind()
        self.title = self.title or text
        self.busy, self.status = True, sc.status_busy()
        t = Turn(self, text)
        self.turns += 1
        threading.Thread(target=t.run, daemon=True).start()

    def on_event(self, kind, value):
        cols = self.cols()
        if kind == "user":
            return self.draw(sc.user(cols, value))
        if kind == "reply":
            return self.draw(sc.reply(cols, value))
        if kind == "tool":
            return self.draw(sc.tool(cols, *value))
        if kind == "error":
            return self.draw(sc.error(cols, value))
        if kind == "ended":
            self.busy, self.status = False, sc.status_done(value)
            self.draw()
            if self.queued:
                self.submit(self.queued.pop(0))

    def run(self):
        fd = sys.stdin.fileno()
        saved = termios.tcgetattr(fd)
        tty.setraw(fd)
        for sig in (signal.SIGTERM, signal.SIGHUP):
            signal.signal(sig, lambda *_: os.write(self.wake_w, b"q"))
        signal.signal(signal.SIGWINCH, lambda *_: os.write(self.wake_w, b"w"))
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        keys = Keys()
        try:
            self.out(sc.setup(self.fixture))
            self.servers = peers.servers(self.home, self.log)
            self.tool_specs = peers.tool_specs(self.servers)
            self.open_session()
            sid = self.session.id if self.session else ""
            self.draw(sc.welcome(self.cols(), self.cwd, sid, self.model, self.opts["version"],
                                 [(x.name, len(x.tools)) for x in self.servers]))
            while True:
                ready = select.select([fd, self.wake_r], [], [])[0]
                if self.wake_r in ready:
                    woke = os.read(self.wake_r, 4096)
                    if b"q" in woke:
                        return 0
                    while not self.events.empty():
                        self.on_event(*self.events.get())
                    if b"w" in woke:
                        self.draw()
                if fd in ready:
                    data = os.read(fd, 65536)
                    if not data:
                        return 0
                    for action, text in keys.feed(data, self.typed):
                        if action == "eof":
                            return 0
                        self.typed = text
                        if action == "submit":
                            self.typed = ""
                            if text.strip():
                                self.submit(text.strip())
                    self.draw()
        except SystemExit:
            return 0
        finally:
            # the pty may be gone (EIO) when the pane closed: release what
            # outlives this process first, the terminal last
            for s in self.servers:
                s.close()
            if self.session:
                self.session.unlock()
            try:
                self.out(sc.goodbye(getattr(self.live, "below", 0), self.session.id if self.session else None))
                termios.tcsetattr(fd, termios.TCSADRAIN, saved)
            except OSError:
                pass


class Keys:
    """Bytes from the terminal into edits of the input line.  Escape
    sequences (the terminal's answers to the setup queries, focus
    reports, keys this does not use) are passed over; a bracketed paste
    is taken whole, its line breaks kept."""

    def __init__(self):
        self.pending, self.paste = "", None
        self.dec = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, data, typed):
        s = self.pending + self.dec.decode(data)
        self.pending = ""
        out, i = [], 0
        while i < len(s):
            if self.paste is not None:
                end = s.find("\x1b[201~", i)
                if end < 0:
                    self.paste += s[i:]
                    break
                typed += (self.paste + s[i:end]).replace("\r\n", "\n").replace("\r", "\n")
                self.paste, i = None, end + 6
                out.append(("edit", typed))
                continue
            c = s[i]
            if c == "\x1b":
                n = skip_escape(s, i)
                if n is None:
                    self.pending = s[i:]
                    break
                if s.startswith("\x1b[200~", i):
                    self.paste = ""
                i = n
                continue
            i += 1
            if c in "\r\n":
                out.append(("submit", typed))
                typed = ""
            elif c in "\x7f\x08":
                typed = typed[:-1]
            elif c in "\x03\x15":
                typed = ""
            elif c == "\x04" and not typed:
                out.append(("eof", ""))
            elif c >= " ":
                typed += c
            if c not in "\r\n":
                out.append(("edit", typed))
        return out


def skip_escape(s, i):
    """Where the escape sequence at s[i] ends, None when it is cut off."""
    if i + 1 >= len(s):
        return None
    k = s[i + 1]
    if k == "[":
        j = i + 2
        while j < len(s) and not ("\x40" <= s[j] <= "\x7e"):
            j += 1
        return j + 1 if j < len(s) else None
    if k in "]P_^":
        ends = [(j, len(e)) for e in ("\x07", "\x1b\\") for j in [s.find(e, i + 2)] if j >= 0]
        return sum(min(ends)) if ends else None
    return i + 2
