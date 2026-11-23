#!/usr/bin/env python3
"""The six latencies of the pane control socket, against a running
sandbox app (bin/measure-pane-control.sh starts it).

    _pane_control_probe.py SOCKET RAW_PANE SHELL_PANE OUT.json

RAW_PANE runs a program that reads its input and prints nothing;
SHELL_PANE sits at a /bin/sh prompt.  Each latency is timed from the
request leaving this process to its answer arriving, on connections
kept open, paced under the socket's per-caller rate (20 a second).

  send          pane.send of one byte, no Enter (written to the PTY)
  send_enter    the same with Enter (Enter written; includes the 50 ms
                the session leaves for the echo)
  fired         a `change` wait, from the answer to the send that makes
                the pane print to the answer to the wait
  run_true      pane.run true, to the answer carrying the exit status;
                paced past the 150 ms a run's shell gets to redraw its
                prompt, which only input that follows a run that closely
                waits out
  read          pane.read without history (asks the session nothing)
  open_shadow   pane.open of a shadow pane to its shell running a
                command; null when the core does not open shadow panes
"""
import json
import os
import socket
import sys
import threading
import time

ROUNDS = int(os.environ.get("PANECTL_ROUNDS", "100"))
PACE = 0.06


class Conn:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(path)
        self.f = self.s.makefile("rb")

    def ask(self, op, **args):
        line = json.dumps({"v": 1, "op": op, "args": args}) + "\n"
        t0 = time.perf_counter()
        self.s.sendall(line.encode())
        ans = json.loads(self.f.readline())
        return (time.perf_counter() - t0) * 1000.0, ans


def p95(xs):
    s = sorted(xs)
    return s[min(len(s) - 1, int(round(0.95 * (len(s) - 1))))] if s else None


def need(ans, what):
    if not ans.get("ok"):
        raise SystemExit(f"{what}: {ans}")
    return ans["result"]


def timed(conn, n, op, pace=PACE, **args):
    xs = []
    for _ in range(n):
        ms, ans = conn.ask(op, **args)
        need(ans, op)
        xs.append(ms)
        time.sleep(pace)
    return xs


def fired(path, pane, n):
    """A wait for `change`; the send that makes the shell echo one byte."""
    waiter, sender, xs = Conn(path), Conn(path), []
    for i in range(n):
        box = {}
        def wait():
            _, ans = waiter.ask("pane.read", target=pane, wait={"change": True}, timeout_s=10)
            box["at"] = time.perf_counter()
            box["ans"] = ans

        t = threading.Thread(target=wait)
        t.start()
        time.sleep(0.05)
        need(sender.ask("pane.send", target=pane, text="x", enter=False)[1], "send")
        sent = time.perf_counter()
        t.join()
        need(box["ans"], "wait")
        xs.append(max(0.0, (box["at"] - sent) * 1000.0))
        if i % 20 == 19:
            need(sender.ask("pane.send", target=pane, keys=["ctrl-u"])[1], "clear line")
        time.sleep(PACE * 3)
    need(sender.ask("pane.send", target=pane, keys=["ctrl-u"])[1], "clear line")
    return xs


def open_shadow(conn, parent, n):
    xs = []
    for _ in range(n):
        t0 = time.perf_counter()
        sid = need(conn.ask("pane.open", parent=str(parent), shadow=True)[1], "open")["id"]
        while True:
            _, ans = conn.ask("pane.run", target=str(sid), command="true", timeout_s=5)
            if ans.get("ok") and ans["result"].get("exit_status") == 0:
                break
            time.sleep(0.02)
        xs.append((time.perf_counter() - t0) * 1000.0)
        need(conn.ask("pane.close", target=str(sid))[1], "close")
        time.sleep(1.0)
    return xs


def load1():
    return os.getloadavg()[0]


def main():
    path, raw, sh, out = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
    caps = int(os.environ.get("PANECTL_CAPS", "0"))
    loads = [load1()]
    c = Conn(path)
    rows = {}
    rows["send"] = timed(c, ROUNDS, "pane.send", target=raw, text="x", enter=False)
    loads.append(load1())
    rows["send_enter"] = timed(c, ROUNDS, "pane.send", target=raw, text="x", enter=True)
    loads.append(load1())
    rows["fired"] = fired(path, sh, ROUNDS // 2)
    loads.append(load1())
    rows["run_true"] = timed(c, ROUNDS // 2, "pane.run", pace=0.2, target=sh, command="true", timeout_s=10)
    loads.append(load1())
    rows["read"] = timed(c, ROUNDS, "pane.read", target=sh)
    loads.append(load1())
    rows["open_shadow"] = open_shadow(c, sh, 10) if caps & 2 else None
    loads.append(load1())
    doc = {k: (None if v is None else {"p95_ms": round(p95(v), 3), "samples_ms": [round(x, 3) for x in v]}) for k, v in rows.items()}
    doc.update({"caps": caps, "load1_start": loads[0], "load1_max": max(loads), "load1_end": loads[-1]})
    with open(out, "w") as f:
        json.dump(doc, f, indent=1)
    for k, v in doc.items():
        if isinstance(v, dict):
            print(f"pane-control {k} p95 {v['p95_ms']} ms ({len(v['samples_ms'])} samples)")
        elif v is None:
            print(f"pane-control {k} skipped (the core does not open shadow panes)")


if __name__ == "__main__":
    main()
