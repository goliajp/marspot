#!/usr/bin/env python3
"""A pane's worth of coding-agent screen that does nothing but keep its
status line alive, for bin/test-awake-cost.sh.

Draws a short transcript once, then every second rewrites the bottom
row the way an agent's spinner and status bar do: save the cursor, move
to the last row, clear it, write the line, restore the cursor.  A
single long-lived process that sleeps between frames, as the real ones
are, so what the terminal pays is the redraw and not process churn.

Each tick is counted in <ticks-dir>/<pid> (the count, rewritten), so
the suite can tell afterwards that every pane was drawing for the whole
window and a quiet reading is not a dead agent.
"""
import os
import sys
import time

ticks_dir = sys.argv[1]
os.makedirs(ticks_dir, exist_ok=True)
tick_path = os.path.join(ticks_dir, str(os.getpid()))
out = sys.stdout

out.write("\x1b[2J\x1b[H")
out.write("> refactor the parser so the bytes path allocates nothing\n\n")
out.write("  Reading src/parser.rs\n  Reading src/grid.rs\n  Editing src/parser.rs\n")
out.flush()

spinner = "|/-\\"
start = time.monotonic()
next_at = start
n = 0
while True:
    rows = os.get_terminal_size(out.fileno()).lines
    secs = int(time.monotonic() - start)
    line = f"{spinner[n % 4]} Working... ({secs}s, {1200 + 37 * n} tokens, esc to interrupt)"
    out.write(f"\x1b7\x1b[{rows};1H\x1b[2K\x1b[2m{line}\x1b[0m\x1b8")
    out.flush()
    n += 1
    # replaced whole, so a kill between truncate and write leaves no empty file
    with open(tick_path + ".tmp", "w") as f:
        f.write(f"{n}\n")
    os.replace(tick_path + ".tmp", tick_path)
    # one a second; a stall is not made up with a burst of frames
    next_at = max(next_at + 1, time.monotonic())
    time.sleep(next_at - time.monotonic())
