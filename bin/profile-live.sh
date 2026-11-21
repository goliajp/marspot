#!/usr/bin/env bash
# bin/profile-live.sh — sampling profile of marspot during a workload.
#
# Why this exists: the perf notes record a 2.4× live-vs-headless gap on
# cat-ascii (89 MiB/s live, 215 MiB/s headless --bench parse).  The
# remaining gap is inside the live pipeline — PTY syscall, reader
# thread, parser, render — and we want a function-level breakdown
# to know where to optimise.
#
# This is a *diagnostic* tool, not a recurring bench scenario.  Run
# it manually when you want to know "where is marspot spending time
# during a sustained burst?"  Output is a one-shot text report; the
# top hot frames go into the perf notes as quoted findings.
#
# Method:
#   1. Launch the product in a bench sandbox (bin/drivers/marspot.sh
#      run-app: shell, core, one session for one pane) whose pane shell
#      runs `cat cat-ascii.bin` in a loop (so the workload sustains for
#      the full sample duration).
#   2. Wait for the pane's marspot-session (or, with PROFILE_PROC=core,
#      the marspot-core) to come up and take its PID.
#   3. Run `sample <pid> <duration>` — macOS's built-in sampling
#      profiler.  Writes a textual stack-sample report.
#   4. Stop the sandbox.
#   5. Print the top hot frames.
#
# Usage:
#   bin/profile-live.sh [duration_s=5]
#   PROFILE_PROC=core bin/profile-live.sh   # sample the core instead

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"

DURATION_S=${1:-5}

SCN="$SCENARIOS_DIR/cat-ascii.bin"
[[ -f "$SCN" ]] || { "$ROOT/bin/gen-scenarios.sh" >/dev/null; }

PROC="marspot-${PROFILE_PROC:-session}"
for b in marspot-shell marspot-core marspot-session; do
  if [[ ! -x "$(marspot_bin "$b")" ]]; then
    ( cd "$ROOT" && cargo build --release --bin "$b" 2>&1 | tail -3 )
  fi
done

RUN_DIR="$MARKER_PREFIX/profile-$$"
rm -rf "$RUN_DIR"; mkdir -p "$RUN_DIR"
# Sample output goes to a persistent location so the user can review
# the full report after the script exits — RUN_DIR is cleaned up.
mkdir -p "$RESULTS_DIR/profiles"
SAMPLE_OUT="$RESULTS_DIR/profiles/sample-$(date +%Y%m%d-%H%M%S).txt"

WORKER="$RUN_DIR/worker.sh"
cat > "$WORKER" <<EOF
#!/bin/sh
# Loop the cat scenario continuously so the pane keeps draining for the
# full sample window.  Each iteration is ~0.3 s on this machine, so
# 5 s of sampling sees ~15-20 iterations.
i=0
while [ \$i -lt 50 ]; do
  /bin/cat "$SCN" > /dev/null 2>&1 || true
  i=\$((i+1))
done
sleep 1
exit 0
EOF
chmod +x "$WORKER"

USER_APP=$(current_frontmost_app)
trap '{ kill_app marspot 2>/dev/null; rm -rf "$RUN_DIR"; restore_focus_to "$USER_APP"; } || true' EXIT INT TERM

echo "==> launching the product with a continuous cat-ascii workload"
kill_app marspot || true
"$ROOT/bin/drivers/marspot.sh" run-app "$WORKER"

# Wait for the process to sample to come up.
target_pid=""
for _ in $(seq 1 50); do
  for p in $(pids_of marspot); do
    if [[ $(ps -o comm= -p "$p" 2>/dev/null) == *"/$PROC" ]]; then target_pid=$p; break; fi
  done
  [[ -n "$target_pid" ]] && break
  sleep 0.2
done
if [[ -z "$target_pid" ]]; then
  echo "profile-live: $PROC failed to start" >&2
  exit 1
fi

# Restore user focus immediately — sampling doesn't need the window to
# be frontmost.
restore_focus_to "$USER_APP"

echo "==> sampling $PROC pid=$target_pid for ${DURATION_S}s"
/usr/bin/sample "$target_pid" "$DURATION_S" -file "$SAMPLE_OUT" -mayDie >/dev/null 2>&1 || true

kill_app marspot || true

if [[ ! -s "$SAMPLE_OUT" ]]; then
  echo "profile-live: sample produced no output" >&2
  exit 1
fi

# ---- summarize ------------------------------------------------------
#
# `sample`'s textual format is hierarchical:
#   <count>  <symbol>  (in <module>)
#     <count>  <symbol>  (in <module>)
#     ...
# The leaf rows are the most actionable hot frames.  We extract them
# and rank by sample count.

echo
echo "==> sample report saved to $SAMPLE_OUT"
echo
echo "==> top 25 leaf frames (most-time-spent at the bottom of stacks):"
echo

python3 - "$SAMPLE_OUT" 25 "$PROC" <<'PY'
import re, sys
from collections import defaultdict

path, top_n, proc = sys.argv[1], int(sys.argv[2]), sys.argv[3]
hot = defaultdict(int)

# `sample`'s call-graph format uses an indent built of spaces *and*
# the chars `+`, `!`, `:`, `|` that draw the tree.  We consider the
# *indent depth* to be the column of the first digit, then digits +
# space + symbol.
lines = []
with open(path, errors="replace") as f:
    in_callgraph = False
    for line in f:
        if "Call graph:" in line:
            in_callgraph = True
            continue
        if not in_callgraph:
            continue
        if line.startswith("Total number"):
            break
        m = re.match(r'^([\s+!:|]*)(\d+)\s+(.+?)(?:\s+\[0x[0-9a-fA-F]+\])?$',
                     line.rstrip("\n"))
        if not m:
            continue
        indent = len(m.group(1))
        count = int(m.group(2))
        symbol = m.group(3).strip()
        lines.append((indent, count, symbol))

# A node is a *leaf* if the next non-empty line has indent <= its own.
for i, (indent, count, symbol) in enumerate(lines):
    nxt_indent = lines[i+1][0] if i+1 < len(lines) else -1
    if nxt_indent <= indent:
        hot[symbol] += count

ranked = sorted(hot.items(), key=lambda kv: -kv[1])
total = sum(hot.values()) or 1
for sym, count in ranked[:top_n]:
    pct = 100.0 * count / total
    # Trim "load address …" from rows where symbols are unresolved
    # (release binary stripped) — keep the offset for cross-reference.
    sym = re.sub(r'load address 0x[0-9a-f]+ \+ ', proc + '+', sym)
    print(f"  {count:>5} ({pct:5.1f}%)  {sym}")
PY

echo
echo "(Full report: $SAMPLE_OUT — keep until the findings are written up)"
exit 0
