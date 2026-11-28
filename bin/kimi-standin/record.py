#!/usr/bin/env python3
"""Records one exchange with a real Kimi Code TUI for the stand-in:
what it draws at start, around three prompts (a plain one held back by
the model stand-in, one answered with a tool call, one the service
refuses for quota) and on /exit, with the files it wrote: the session
record, the index, a lock taken mid-turn, what its hooks and its MCP
server were given.

Usage: record.py <kimi> <kimi-home> <cwd> <probe-log-dir> <out.json>
Run by bin/record-kimi-exchange.sh, which sets up the throwaway home
and refuses on a runner.  Writes the raw recording; sanitize.py makes
the fixture of it.
"""
import codecs
import fcntl
import glob
import json
import os
import pty
import select
import signal
import struct
import subprocess
import sys
import termios
import time

KIMI, HOME, CWD, PROBES, OUT = sys.argv[1:6]
ROWS, COLS = 40, 120
PROMPTS = [("plain", "SLOW say hello to the recording"),
           ("tool", "Send probe the text RECORDED with context"),
           ("quota", "QUOTA-EXHAUSTED now")]
# what a terminal answers the queries a TUI sends at start
ANSWERS = [(b"\x1b[6n", b"\x1b[1;1R"), (b"\x1b[c", b"\x1b[?62;22c"),
           (b"\x1b]11;?\x07", b"\x1b]11;rgb:0000/0000/0000\x07"),
           (b"\x1b]11;?\x1b\\", b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
           (b"\x1b]10;?\x07", b"\x1b]10;rgb:ffff/ffff/ffff\x07"),
           (b"\x1b]10;?\x1b\\", b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
           (b"\x1b[?u", b"\x1b[?0u")]

m, s = pty.openpty()
fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
p = subprocess.Popen([KIMI, "--yolo"], stdin=s, stdout=s, stderr=s, cwd=CWD, start_new_session=True,
                     env=dict(os.environ, KIMI_CODE_HOME=HOME, TERM="xterm-256color"))
os.close(s)
dec = codecs.getincrementaldecoder("utf-8")("replace")
t0 = time.time()
segments = []
current = {"name": "start", "out": []}
seen = b""
asked = []
locks = {}


def wire():
    found = glob.glob(os.path.join(HOME, "sessions", "*", "*", "agents", "main", "wire.jsonl"))
    return open(found[0]).read() if found else ""


def ended():
    return wire().count('"type":"turn.ended"')


def snapshot_locks():
    for f in glob.glob(os.path.join(HOME, "locks", "**", "*"), recursive=True):
        if os.path.isfile(f) and f not in locks:
            try:
                locks[f] = open(f).read()
            except OSError:
                pass


def pump(quiet, limit, until=None):
    """Read until `until()` holds and `quiet` seconds pass with nothing
    drawn, or `limit`."""
    global seen
    last = time.time()
    end = time.time() + limit
    while time.time() < end:
        snapshot_locks()
        if time.time() - last >= quiet and (until is None or until()):
            break
        if p.poll() is not None:
            break
        if not select.select([m], [], [], 0.1)[0]:
            continue
        try:
            b = os.read(m, 65536)
        except OSError:
            break
        last = time.time()
        seen = (seen + b)[-64:]
        for q, a in ANSWERS:
            if q in seen:
                os.write(m, a)
                asked.append(q.decode())
                seen = seen.replace(q, b"")
        current["out"].append([round(time.time() - t0, 3), dec.decode(b)])


def mark(name, typed):
    global current
    segments.append(current)
    current = {"name": name, "out": [], "typed": typed}
    os.write(m, typed.encode())
    time.sleep(0.3)
    os.write(m, b"\r")


def end_group():
    """kimi runs in its own session, so its MCP servers and hooks are
    in its group: ended together, or they stay behind on this machine"""
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(p.pid, sig)
        except ProcessLookupError:
            return
        try:
            p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass


# a `timeout` above this script sends SIGTERM: leave through the finally
signal.signal(signal.SIGTERM, lambda *_: sys.exit(124))
try:
    pump(3, 40)
    for name, prompt in PROMPTS:
        before = ended()
        mark(name, prompt)
        pump(2, 60, lambda: ended() > before)
    mark("exit", "/exit")
    pump(3, 20)
    segments.append(current)
finally:
    end_group()


def read(path):
    try:
        return open(path).read()
    except OSError:
        return None


doctor = subprocess.run([KIMI, "doctor"], capture_output=True, text=True, timeout=60,
                        env=dict(os.environ, KIMI_CODE_HOME=HOME), cwd=CWD)
version = subprocess.run([KIMI, "--version"], capture_output=True, text=True, timeout=30)
files = sorted(os.path.relpath(f, HOME) for f in glob.glob(os.path.join(HOME, "**", "*"), recursive=True))
with open(OUT, "w") as f:
    json.dump({"rows": ROWS, "cols": COLS, "exit": p.returncode, "asked": asked, "segments": segments,
               "wire": wire(), "index": read(os.path.join(HOME, "session_index.jsonl")), "locks": locks,
               "hooks": read(os.path.join(PROBES, "hook.log")), "mcp": read(os.path.join(PROBES, "mcp.log")),
               "doctor": {"code": doctor.returncode, "out": doctor.stdout, "err": doctor.stderr},
               "version": version.stdout.strip(), "files": files}, f, indent=1)
