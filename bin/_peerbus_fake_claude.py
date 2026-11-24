#!/usr/bin/env python3
"""A process that registers itself the way a Claude Code session does,
for the peerbus end-to-end test: one record in <registry>, named the
way Claude names a session from its directory, and an inbox socket
that takes whatever is written to it.

Usage: _peerbus_fake_claude.py <registry-dir> <seconds> [<log-file>]

Every line that arrives on the inbox is appended to <log-file>.  The
record and the socket are removed when the time is up.

FAKE_SIZE_LOG in the environment names a file that gets the terminal's
size, `<epoch seconds> <cols> <rows>`, at the start and at each SIGWINCH.
With FAKE_ALT the session is on the alternate screen.  With FAKE_KEYS_LOG
it draws Claude Code's empty input box, redrawn at each SIGWINCH, takes
no key, and appends what it reads from the terminal to that file, one
read a line, as hex.
"""
import fcntl
import json
import os
import select
import signal
import socket
import struct
import subprocess
import sys
import termios
import time
import tty
import uuid

def ps(field):
    return subprocess.run(["ps", "-o", field + "=", "-p", str(os.getpid())], env={"LC_ALL": "C", "TZ": "UTC"},
                          capture_output=True, text=True).stdout.strip()


# marspot names a pane's Claude session only when the program in front
# of the pane is `claude` by argv[0], so take that name: exec the
# interpreter image itself (`comm`), as a launcher such as macOS's
# python3 would re-exec under its own name, keeping this pid
if os.path.basename(ps("args").split(" ")[0]) != "claude":
    os.execv(ps("comm"), ["claude"] + sys.argv)

registry, seconds = sys.argv[1], float(sys.argv[2])
log = sys.argv[3] if len(sys.argv) > 3 else os.devnull
pid = os.getpid()
lstart = ps("lstart")
now = int(time.time() * 1000)
path = os.path.join(registry, "%d.json" % pid)
inbox = "/tmp/cc-socks/e2e-fake-%d.sock" % pid
os.makedirs(os.path.dirname(inbox), exist_ok=True)
listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(inbox)
listener.listen(8)
listener.settimeout(0.5)
with open(path, "w") as f:
    json.dump({"pid": pid, "sessionId": str(uuid.uuid4()), "cwd": os.getcwd(), "startedAt": now,
               "procStart": lstart, "version": "2.1.308", "peerProtocol": 1,
               "peerFeatures": ["notify_idle"], "kind": "interactive", "entrypoint": "cli",
               "pidDomain": "darwin", "messagingSocketPath": inbox,
               "name": "fake-claude-6a", "nameSource": "derived", "nameSince": now,
               "status": "idle", "updatedAt": now, "statusUpdatedAt": now}, f)


KEYS = os.environ.get("FAKE_KEYS_LOG")


def draw_box(cols):
    rule = "\u2500" * cols
    sys.stdout.write("\x1b[H\x1b[2J%s\r\n\u276f \r\n%s\r\n  ? for shortcuts\x1b[2;3H" % (rule, rule))
    sys.stdout.flush()


def note_size(*_):
    rows, cols, _, _ = struct.unpack("HHHH", fcntl.ioctl(0, termios.TIOCGWINSZ, b"\0" * 8))
    if KEYS:
        draw_box(cols)
    if "FAKE_SIZE_LOG" in os.environ:
        with open(os.environ["FAKE_SIZE_LOG"], "a") as sizes:
            sizes.write("%.3f %d %d\n" % (time.time(), cols, rows))


if "FAKE_ALT" in os.environ:
    sys.stdout.write("\x1b[?1049h\x1b[H")
print("fake claude %d registered" % pid, flush=True)
if KEYS:
    saved = termios.tcgetattr(0)
    tty.setraw(0)
if "FAKE_SIZE_LOG" in os.environ or KEYS:
    note_size()
    signal.signal(signal.SIGWINCH, note_size)
# a kill still removes the record and the socket
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
deadline = time.time() + seconds
try:
    while time.time() < deadline:
        try:
            ready = select.select([listener, 0] if KEYS else [listener], [], [], 0.5)[0]
        except InterruptedError:
            continue
        if 0 in ready:
            with open(KEYS, "a") as keys:
                keys.write(os.read(0, 64).hex() + "\n")
        if listener not in ready:
            continue
        conn, _ = listener.accept()
        conn.settimeout(5.0)
        data = b""
        try:
            while True:
                chunk = conn.recv(65536)
                if not chunk:
                    break
                data += chunk
        except socket.timeout:
            pass
        conn.close()
        with open(log, "ab") as f:
            f.write(data)
finally:
    if KEYS:
        termios.tcsetattr(0, termios.TCSADRAIN, saved)
    os.unlink(path)
    os.unlink(inbox)
