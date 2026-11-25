#!/usr/bin/env python3
"""bin/_active_stream.py -- the "active" pane of bin/measure-active.sh.

A bounded, deterministic stand-in for what an agent CLI puts on screen
while it works: a status line with a spinner rewritten ten times a
second, response text streamed a few words per tick and wrapped into
lines above it, and every eight seconds a boxed tool block of thirty
coloured lines in one burst.  About 1.5 KiB/s on average.  The tool
names repeat every four bursts, so a measured window that is a whole
number of CYCLE_SECS (64 s, eight bursts) holds the same number of
bursts of each kind wherever in the stream it starts.

The schedule is absolute (tick k is due at start + k/10 s), so every
terminal is handed the same bytes at the same pace; a terminal that
blocks the pty shows up as late ticks in the stats, not as less output.

  --go FIFO     block on opening it, then read the duration in seconds
                from it: the measurement decides when the stream starts
  --stats FILE  written when the stream ends: bytes, ticks, lateness,
                and the cumulative bytes after every tick with its wall
                time, so a caller can count what fell inside its window
"""
import argparse
import json
import os
import random
import time

TICK = 0.1
BURST_TICKS = 80
CYCLE_SECS = 64
SPIN = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"
WORDS = (
    "the parser reads each byte once and keeps no state outside the grid so a resize only "
    "touches the rows that are visible while scrollback stays on disk until someone asks for "
    "it the session owns its pty and the core only sees frames through shared memory "
    "解析器 每个 字节 只读 一次 渲染 只在 内容 变化 时 发生 空闲 时 不 唤醒"
).split()
TOOL = ("Bash(cargo test -p marspot-term)", "Read(src/grid.rs)", "Edit(src/render_metal.rs)",
        "Grep(\"fn apply\")")
WIDTH = 76


def out(fd, data, stats):
    b = data.encode()
    stats["bytes"] += len(b)
    while b:
        n = os.write(fd, b)
        b = b[n:]


def status(tick, tokens):
    return (f"\r\x1b[2K\x1b[38;5;208m{SPIN[tick % len(SPIN)]}\x1b[0m Working… "
            f"\x1b[2m({tick // 10}s · ↓ {tokens} tokens · esc to interrupt)\x1b[0m")


def tool_block(rng, n):
    name = TOOL[n % len(TOOL)]
    lines = [f"\x1b[32m⏺\x1b[0m \x1b[1m{name}\x1b[0m", "  ╭" + "─" * (WIDTH - 4) + "╮"]
    for i in range(30):
        mark, col = rng.choice((("+", 32), ("-", 31), (" ", 37)))
        body = " ".join(rng.choice(WORDS) for _ in range(8))[: WIDTH - 12]
        lines.append(f"  │ \x1b[2m{i + 1:4d}\x1b[0m \x1b[{col}m{mark} {body}\x1b[0m")
    lines.append("  ╰" + "─" * (WIDTH - 4) + "╯")
    return lines


def run(duration, fd, stats):
    rng = random.Random(21)
    total = int(round(duration / TICK))
    line, tokens, blocks = "", 0, 0
    series = stats["series"]
    start = time.time()
    stats["started_at"] = start
    for tick in range(total):
        due = start + tick * TICK
        now = time.time()
        if due > now:
            time.sleep(due - now)
        else:
            lag = now - due
            stats["max_lag_ms"] = max(stats["max_lag_ms"], round(lag * 1000, 1))
            if lag > TICK / 2:
                stats["late_ticks"] += 1
        done = []
        for _ in range(3):
            w = rng.choice(WORDS)
            tokens += 1
            if len(line) + len(w) + 1 > WIDTH:
                done.append(line)
                line = ""
            line = f"{line} {w}" if line else w
        if tick % BURST_TICKS == BURST_TICKS - 1:
            done += tool_block(rng, blocks)
            blocks += 1
        frame = "".join(f"\r\x1b[2K{l}\r\n" for l in done)
        out(fd, frame + status(tick, tokens), stats)
        stats["ticks"] += 1
        series.append((round(time.time(), 3), stats["bytes"]))
    out(fd, "\r\x1b[2K\r\n", stats)
    stats["elapsed_secs"] = round(time.time() - start, 3)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--go", required=True)
    ap.add_argument("--stats", required=True)
    a = ap.parse_args()
    with open(a.go) as f:
        duration = float(f.readline())
    stats = {"duration_secs": duration, "tick_secs": TICK, "ticks": 0, "bytes": 0,
             "late_ticks": 0, "max_lag_ms": 0.0,
             "burst_secs": BURST_TICKS * TICK, "cycle_secs": CYCLE_SECS, "series": []}
    run(duration, 1, stats)
    tmp = a.stats + ".tmp"
    with open(tmp, "w") as f:
        json.dump(stats, f)
    os.replace(tmp, a.stats)


if __name__ == "__main__":
    main()
