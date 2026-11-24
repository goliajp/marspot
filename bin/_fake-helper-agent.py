#!/usr/bin/env python3
"""A stand-in for claude, codex, gemini and kimi as a helper runs them,
for bin/test-agent-control.sh.  Linked under each agent's name; which
one it is comes from argv[0].

It takes its task the way that agent does: claude after `--`, codex as
its last argument, gemini after `-i`, kimi typed in once its screen is
up (read up to the line holding END-OF-TASK).  Every start is logged to
$FAKE_HELPER_LOG as `name<TAB>task bytes<TAB>arguments`.

The task says what to do:
  FAKE-EXIT    write the result file the task names, then exit
  FAKE-STAY    write it, then draw an empty input box and wait
  FAKE-SILENT  write nothing, draw the box and wait
  FAKE-WORK=f  first print a line a second until the file f is there
  FAKE-BLOCK   ask a question, and wait for an answer
  FAKE-TOKENS  (claude) first register as a Claude session whose last
               response says 115 300 tokens, after a tool call

claude shows its first-run dialogs before that, the cursor on "No, exit"
and with no numbers, when the task holds
  FAKE-TRUST        the trust dialog for the directory it runs in
  FAKE-TRUST-OTHER  the trust dialog for another directory
  FAKE-TRUST-STUCK  the trust dialog, which takes Enter and stays up
  FAKE-BYPASS       the bypass-permissions dialog (after a trust dialog)
  FAKE-LATE         nothing at all for 14 s before each of these
  FAKE-ASK          (with FAKE-STAY) read lines in the box; FAKE-REFUSE in
                    one draws what claude says of a login that is refused
Started with --continue it draws the box and waits.
Each key it reads there is logged as `key<TAB>what`.
"""
import os
import atexit
import json
import re
import subprocess
import sys
import termios
import time
import tty
import uuid

name = os.path.basename(sys.argv[0])
args = sys.argv[1:]


def log(line):
    with open(os.environ["FAKE_HELPER_LOG"], "a") as f:
        f.write(line + "\n")


def dialog(head, yes, stuck=False):
    """Draw it, take keys, and exit on No or Esc as claude does."""
    options, at = ["No, exit", yes], 0
    was = termios.tcgetattr(0)
    tty.setraw(0)
    while True:
        rows = [" " + r for r in head] + [""]
        rows += [(" ❯ " if i == at else "   ") + o for i, o in enumerate(options)]
        rows += ["", " Enter to confirm · Esc to cancel"]
        sys.stdout.write("\x1b[2J\x1b[H" + "\r\n".join(rows))
        sys.stdout.flush()
        key = os.read(0, 16)
        if key in (b"\x1b[B", b"\x1b[A"):
            at = min(max(at + (1 if key == b"\x1b[B" else -1), 0), 1)
            log("key\tarrow to " + options[at])
        elif key == b"\r" and stuck:
            log("key\tenter kept on " + options[at])
        elif key == b"\r":
            log("key\tenter on " + options[at])
            break
        elif key == b"\x1b":
            log("key\tesc")
            at = 0
            break
    termios.tcsetattr(0, termios.TCSANOW, was)
    sys.stdout.write("\x1b[2J\x1b[H")
    sys.stdout.flush()
    if at == 0:
        sys.exit(1)


def trust(where, stuck):
    rows = [where[i:i + 118] for i in range(0, len(where), 118)]
    dialog(["Accessing workspace:", ""] + rows + [
        "",
        "Quick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source",
        "project, or work from your team). If not, take a moment to review what's in this folder first.",
        "",
        "Claude Code'll be able to read, edit, and execute files here.",
        "",
        "Security guide",
    ], "Yes, I trust this folder", stuck)


def bypass():
    dialog([
        "WARNING: Claude Code running in Bypass Permissions mode",
        "",
        "In Bypass Permissions mode, Claude Code will not ask for your approval before running potentially dangerous commands.",
        "",
        "By proceeding, you accept all responsibility for actions taken while running in Bypass Permissions mode.",
    ], "Yes, I accept")


if name == "kimi":
    print("kimi stand-in ready", flush=True)
    lines = []
    while True:
        line = sys.stdin.readline()
        if not line:
            break
        lines.append(line)
        if "END-OF-TASK" in line:
            break
    task = "".join(lines)
elif name == "gemini":
    task = args[args.index("-i") + 1]
elif name == "claude":
    task = args[args.index("--") + 1]
else:
    task = args[-1]

size = len(task.encode())
with open(os.environ["FAKE_HELPER_LOG"], "a") as f:
    f.write("%s\t%d\t%s\n" % (name, size, " ".join(a for a in args if len(a) < 300)))

if name == "claude":
    late = 14 if "FAKE-LATE" in task else 0
    if "FAKE-TRUST" in task:
        time.sleep(late)
        trust("/private/etc" if "FAKE-TRUST-OTHER" in task else os.getcwd(), "FAKE-TRUST-STUCK" in task)
    if "FAKE-BYPASS" in task:
        time.sleep(late)
        bypass()



def ps(field):
    return subprocess.run(["ps", "-o", field + "=", "-p", str(os.getpid())], env={"LC_ALL": "C", "TZ": "UTC"},
                          capture_output=True, text=True).stdout.strip()


def register():
    """The record Claude keeps of a running session, and its transcript."""
    home = os.environ.get("CLAUDE_CONFIG_DIR") or os.path.join(os.environ["HOME"], ".claude")
    sid, now = str(uuid.uuid4()), int(time.time() * 1000)
    project = os.path.join(home, "projects", re.sub(r"[^A-Za-z0-9]", "-", os.getcwd()))
    os.makedirs(project, exist_ok=True)
    os.makedirs(os.path.join(home, "sessions"), exist_ok=True)
    usage = {"input_tokens": 300, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 114000, "output_tokens": 1000}
    said = {"type": "assistant", "isSidechain": False, "message": {"id": "m1", "role": "assistant", "usage": usage,
            "content": [{"type": "tool_use", "id": "t", "name": "Bash", "input": {}}]}}
    asked = {"type": "user", "isSidechain": False, "message": {"role": "user", "content": task[:100]}}
    with open(os.path.join(project, sid + ".jsonl"), "w") as f:
        for r in (asked, said):
            f.write(json.dumps(dict(r, sessionId=sid, cwd=os.getcwd()), separators=(",", ":")) + "\n")
    record = os.path.join(home, "sessions", "%d.json" % os.getpid())
    atexit.register(os.unlink, record)
    with open(record, "w") as f:
        json.dump({"pid": os.getpid(), "sessionId": sid, "cwd": os.getcwd(), "startedAt": now, "procStart": ps("lstart"),
                   "version": "2.1.308", "peerProtocol": 1, "kind": "interactive", "entrypoint": "cli",
                   "pidDomain": "darwin", "status": "busy", "updatedAt": now, "statusUpdatedAt": now}, f,
                  separators=(",", ":"))


if "FAKE-TOKENS" in task:
    # what reads the record takes only a program called claude for one
    if os.path.basename(ps("args").split(" ")[0]) != "claude":
        os.execv(ps("comm"), ["claude"] + sys.argv)
    register()
working = re.search(r"FAKE-WORK=(\S+)", task)
while working and not os.path.exists(working.group(1)):
    print("%s still working" % name, flush=True)
    time.sleep(1)

while "FAKE-BLOCK" in task:
    print("Allow this command to run? [y/n]", flush=True)
    sys.stdin.readline()

found = re.search(r"write your final result to (\S+result\.md)", task)
print("%s stand-in working on %d bytes" % (name, size), flush=True)
time.sleep(1)
if found and "FAKE-SILENT" not in task:
    with open(found.group(1), "w") as f:
        f.write("RESULT-%s-OK bytes=%d\n" % (name, size))
    print("%s wrote its result" % name, flush=True)

if "FAKE-STAY" in task or "--continue" in args or "FAKE-SILENT" in task:
    # a ruled input box with the cursor in it, empty, as an idle agent shows
    sys.stdout.write("\r\n" + "─" * 40 + "\r\n❯ \r\n" + "─" * 40 + "\x1b[1A\x1b[3G")
    sys.stdout.flush()
    while "FAKE-ASK" in task:
        if "FAKE-REFUSE" in sys.stdin.readline():
            box = "\r\n" + "─" * 40 + "\r\n❯ \r\n" + "─" * 40 + "\x1b[1A\x1b[3G"
            sys.stdout.write("\r\n\r\n● Please run /login · API Error: 401" + box)
            sys.stdout.flush()
    while True:
        time.sleep(3600)
