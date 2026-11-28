#!/usr/bin/env python3
"""Records one exchange with a real Codex TUI for the stand-in: the
bytes it draws at start, around one prompt and its answer, and on
/quit, and what its app-server answers a client the way marspot asks.

Usage: record.py <codex> <codex-home> <cwd> <out.json>
Run by bin/record-codex-exchange.sh, which sets up the throwaway home
and refuses on a runner.  Writes the raw recording; the wrapper
sanitizes it.
"""
import codecs
import fcntl
import json
import os
import pty
import select
import struct
import subprocess
import sys
import termios
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wsrpc  # noqa: E402

CODEX, HOME, CWD, OUT = sys.argv[1:5]
ROWS, COLS = 40, 120
PROMPT = "say hello to the recording"

m, s = pty.openpty()
fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
p = subprocess.Popen([CODEX], stdin=s, stdout=s, stderr=s, cwd=CWD, start_new_session=True,
                     env=dict(os.environ, CODEX_HOME=HOME, TERM="xterm-256color"))
os.close(s)
dec = codecs.getincrementaldecoder("utf-8")("replace")
t0 = time.time()
segments = []
current = {"name": "start", "out": []}
seen = b""


def pump(quiet, limit):
    """Read until `quiet` seconds pass with nothing drawn, or `limit`."""
    global seen
    last = time.time()
    end = time.time() + limit
    while time.time() < end and time.time() - last < quiet:
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
        # answer the cursor position query, or the TUI waits for it
        if b"\x1b[6n" in seen:
            os.write(m, b"\x1b[1;1R")
            seen = b""
        current["out"].append([round(time.time() - t0, 3), dec.decode(b)])


def mark(name, typed=None):
    global current
    segments.append(current)
    current = {"name": name, "out": []}
    if typed is not None:
        current["typed"] = typed
        os.write(m, typed.encode())
        time.sleep(0.3)
        os.write(m, b"\r")


def probe(tag):
    sock = os.path.join(HOME, "app-server-control", "app-server-control.sock")
    got = {"tag": tag}
    try:
        c = wsrpc.Client(sock, "recorder", timeout=20)
        loaded = c.call("thread/loaded/list", {})
        got["thread/loaded/list"] = loaded
        ids = loaded.get("result", {}).get("data", [])
        for t in ids[:1]:
            got["thread/read"] = c.call("thread/read", {"threadId": t})
            got["thread/resume"] = c.call("thread/resume", {"threadId": t, "excludeTurns": True})
            got["thread/turns/list"] = c.call("thread/turns/list", {"threadId": t})
            c.c.close()
    except Exception as e:  # recorded as found: the wrapper reports it
        got["error"] = repr(e)
    return got


pump(3, 40)
probes = [probe("after start")]
mark("prompt", PROMPT)
pump(4, 60)
probes.append(probe("after the turn"))
mark("quit", "/quit")
pump(3, 20)
segments.append(current)
if p.poll() is None:
    p.terminate()
    p.wait(timeout=5)
with open(OUT, "w") as f:
    json.dump({"rows": ROWS, "cols": COLS, "prompt": PROMPT, "exit": p.returncode,
               "segments": segments, "probes": probes}, f, indent=1)
