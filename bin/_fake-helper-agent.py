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
"""
import os
import re
import sys
import time

name = os.path.basename(sys.argv[0])
args = sys.argv[1:]

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

found = re.search(r"write your final result to (\S+result\.md)", task)
print("%s stand-in working on %d bytes" % (name, size), flush=True)
time.sleep(1)
if found and "FAKE-SILENT" not in task:
    with open(found.group(1), "w") as f:
        f.write("RESULT-%s-OK bytes=%d\n" % (name, size))
    print("%s wrote its result" % name, flush=True)

if "FAKE-STAY" in task or "FAKE-SILENT" in task:
    # a ruled input box with the cursor in it, empty, as an idle agent shows
    sys.stdout.write("\r\n" + "─" * 40 + "\r\n❯ \r\n" + "─" * 40 + "\x1b[1A\x1b[3G")
    sys.stdout.flush()
    while True:
        time.sleep(3600)
