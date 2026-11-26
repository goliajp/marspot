#!/usr/bin/env python3
"""Claude Code's input box and its /model, for bin/test-model-guard.sh.

Usage: _fake_claude_box.py <settings-file> <submits-log> <keys-log>

Draws Claude Code's input box (a rule, the prompt glyph, a rule) with
what has been typed in it, the cursor after the text.  Printable keys
are typed, Backspace deletes, Ctrl-U clears.  Enter appends the line to
<submits-log> and empties the box; a line that is /model, with or
without a model after it, also writes that model (fable without one)
into <settings-file>, the way Claude Code's /model writes the settings
file every account shares: through its link, replacing the file by
rename.  Every read from the terminal is appended to <keys-log> as hex.

<settings-file> and the logs must be under /tmp or $MARSPOT_STATE_DIR:
anywhere else they may be a real account's, and this exits.
"""
import json
import os
import sys
import termios
import tty


def sandboxed(path):
    real = os.path.realpath(path)
    roots = ["/private/tmp", "/tmp"] + [os.path.realpath(d) for d in [os.environ.get("MARSPOT_STATE_DIR")] if d]
    if not any(real == r or real.startswith(r + "/") for r in roots):
        sys.stderr.write("stand-in refuses to write under %s: not /tmp nor the sandbox\n" % real)
        sys.exit(3)
    return path


settings, submits, keys = (sandboxed(p) for p in sys.argv[1:4])
typed = ""


def draw():
    cols = os.get_terminal_size(1).columns
    rule = "─" * cols
    sys.stdout.write("\x1b[H\x1b[2J%s\r\n❯ %s\r\n%s\r\n  ? for shortcuts\x1b[2;%dH" % (rule, typed, rule, 3 + len(typed)))
    sys.stdout.flush()


def set_model(model):
    real = os.path.realpath(settings)
    try:
        with open(real) as f:
            v = json.load(f)
    except (OSError, ValueError):
        v = {}
    v["model"] = model
    with open(real + ".new", "w") as f:
        json.dump(v, f, indent=2)
        f.write("\n")
    os.replace(real + ".new", real)


def submit(line):
    with open(submits, "a") as f:
        f.write(line + "\n")
    words = line.split()
    if words and words[0] == "/model":
        set_model(words[1] if len(words) > 1 else "fable")


saved = termios.tcgetattr(0)
tty.setraw(0)
try:
    draw()
    while True:
        data = os.read(0, 256)
        if not data:
            break
        with open(keys, "a") as f:
            f.write(data.hex() + "\n")
        for ch in data.decode("utf-8", "replace"):
            if ch == "\r":
                submit(typed)
                typed = ""
            elif ch == "\x7f":
                typed = typed[:-1]
            elif ch == "\x15":
                typed = ""
            elif ch == "\x03":
                sys.exit(0)
            elif ch.isprintable():
                typed += ch
        draw()
finally:
    termios.tcsetattr(0, termios.TCSADRAIN, saved)
