#!/usr/bin/env bash
#
# bin/measure-idle.sh — T4 of the public bench: what a terminal costs
# while you are not typing.
#
# This is the layer nobody else publishes, and the one that decides
# whether a machine with twenty sessions open is usable.  A terminal
# that parses fast and then burns a core redrawing cursors has not
# earned the benchmark it quotes.
#
# What it measures, for one app at a time:
#   - CPU seconds consumed by the app's WHOLE process tree over an idle
#     window, reported as a share of one core
#   - resident memory of that tree at the end of the window
#   - both per process, so "which half is it" is answerable
#
# Why by binary path and not by parentage: marspot's per-pane L3
# processes deliberately outlive their parents (silent update, crash
# reattach), so they are reparented to launchd and a descendant walk
# misses exactly the processes this measures.  Every app's set is
# matched on the paths it runs from.
#
# The comparison is only fair if both sides are idle in the same sense,
# so this refuses rather than guesses.  Exit codes, one meaning each,
# the same four as bin/_idle_wakeups.py and bin/measure-idle-marspot.sh:
#   0  measured
#   1  measured, and over a budget -- never from here: this script is
#      given no budget
#   2  the instrument failed, there is no reading: bad usage, the app
#      is not running, fewer shells than `--sessions` asks for, the
#      process set changed during the window, a pid was reused, the
#      window did not actually elapse
#   3  not measured: the host was busy.  The number is written anyway,
#      with the load beside it -- it is still good for an A/B on one
#      machine -- but it is not what the app costs when idle, and a
#      caller must read 3 as neither a pass nor a regression
# A changed set is the one that matters: a pane opening or a helper
# dying mid-window makes the CPU delta a number with no meaning, and
# it is invisible in the result.
#
# Busy is load1 over IDLE_BUSY_LOAD (default 5.0, the line
# bin/bench-remote.sh draws for a host that has company) at any sample
# of the window.  The line used to be 1.0, which the runner is over
# whenever anything waits on its lock: the windows taken there under
# the exclusive lock on 2026-11-12 read load1 1.0 to 3.2, and nearly
# every one of them would have been refused.
#
# Usage:
#   bin/measure-idle.sh --app marspot --sessions 9 [--window 60]
#   bin/measure-idle.sh --list            # what it knows how to find
#
# Other terminals are opened by hand -- the same posture as
# bin/measure-other.sh, for the same reason (AppleScript driving is too
# fragile to bench with).  Open N panes, leave the window alone, run
# this.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RESULTS_DIR="$ROOT/bench/results"

APP=""
SESSIONS=""
WINDOW="${MARSPOT_IDLE_WINDOW:-60}"
FOCUS="unknown"

# app -> the ERE its processes' command lines match.  Anchored on the
# bundle or install path so a checkout's build never counts as the
# installed app and vice versa.
app_pattern() {
  case "$1" in
    marspot)       echo '(\.local/Marspot\.app/Contents/MacOS/marspot-shell|Application Support/marspot/binaries/[^/]+/marspot-(core|session))' ;;
    marspot-dev)   echo "$ROOT/target/(debug|release)/marspot(-shell|-core|-session)?( |\$)" ;;
    iterm2)        echo 'iTerm\.app/Contents/MacOS/iTerm2' ;;
    terminal)      echo 'Terminal\.app/Contents/MacOS/Terminal' ;;
    warp)          echo 'Warp\.app/Contents/MacOS/(stable|Warp)' ;;
    ghostty)       echo 'Ghostty\.app/Contents/MacOS/ghostty' ;;
    alacritty)     echo 'Alacritty\.app/Contents/MacOS/alacritty' ;;
    kitty)         echo 'kitty\.app/Contents/MacOS/kitty' ;;
    *)             return 1 ;;
  esac
}

KNOWN="marspot marspot-dev iterm2 terminal warp ghostty alacritty kitty"

die() { echo "measure-idle: $*" >&2; exit 2; }

while (( $# )); do
  case "$1" in
    --app)      APP="${2:-}"; shift 2 ;;
    --sessions) SESSIONS="${2:-}"; shift 2 ;;
    --window)   WINDOW="${2:-}"; shift 2 ;;
    --focused)  FOCUS="focused"; shift ;;
    --unfocused) FOCUS="unfocused"; shift ;;
    --list)     echo "$KNOWN" | tr ' ' '\n'; exit 0 ;;
    -h|--help)  sed -n '2,56p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)          die "unknown arg: $1" ;;
  esac
done

[[ -n "$APP" ]] || die "need --app (one of: $KNOWN)"
[[ -n "$SESSIONS" ]] || die "need --sessions N -- the number of panes open, which is checked, not assumed"
PATTERN="$(app_pattern "$APP")" || die "unknown app '$APP' (known: $KNOWN)"

# `ps -o cputime` is MM:SS.cc, or HH:MM:SS.cc past the hour.
cputime_to_secs() {
  python3 - "$1" <<'PY'
import sys
t = sys.argv[1].strip()
if not t:
    print("")
    raise SystemExit
parts = t.split(':')
secs = 0.0
for p in parts:
    secs = secs * 60 + float(p)
print(f"{secs:.2f}")
PY
}

# pid cputime rss, one per line, for every process in the set.
sample() {
  ps -Ao pid=,cputime=,rss=,command= \
    | grep -E "$PATTERN" \
    | grep -v "measure-idle" \
    | awk '{printf "%s %s %s\n", $1, $2, $3}'
}

# Shells running under the set, which is how "N panes are open" is
# checked rather than trusted.  A pane without a shell is not a pane.
count_shells() {
  local pids e s n=0
  pids=$(sample | awk '{print $1}' | paste -sd, -)
  [[ -n "$pids" ]] || { echo 0; return; }
  # marspot has launchd start a pane's shell, so it is nobody's child:
  # the session registry names it
  if [[ $APP == marspot* ]]; then
    for e in "${MARSPOT_STATE_DIR:-$HOME/Library/Application Support/marspot}"/sessions/*/entry.toml; do
      s=$(sed -n 's/^shell_child_pid = \([0-9][0-9]*\)$/\1/p' "$e" 2>/dev/null)
      [[ ",$pids," == *",$(sed -n 's/^pid = \([0-9][0-9]*\)$/\1/p' "$e" 2>/dev/null),"* ]] || continue
      [[ -n $s ]] && kill -0 "$s" 2>/dev/null && n=$((n + 1))
    done
    echo "$n"; return
  fi
  ps -Ao ppid=,command= \
    | awk -v want="$pids" '
        BEGIN { n = split(want, a, ","); for (i = 1; i <= n; i++) p[a[i]] = 1 }
        ($1 in p) && ($2 ~ /(zsh|bash|fish|sh|login)$/) { c++ }
        END { print c + 0 }'
}

mkdir -p "$RESULTS_DIR"
FIRST="$(sample)"
[[ -n "$FIRST" ]] || die "no processes match $APP -- is it running?"

SHELLS="$(count_shells)"
if (( SHELLS < SESSIONS )); then
  die "found $SHELLS shell(s) under $APP but --sessions says $SESSIONS; open the panes first"
fi

echo "==> $APP: ${SESSIONS} pane(s), $(echo "$FIRST" | wc -l | tr -d ' ') process(es), idling ${WINDOW}s ($FOCUS)"
# What the rest of the host did while we were calling it idle.  An
# idle-cost measurement taken next to someone else's compile is not a
# measurement of idle cost: the scheduler charges our processes for
# cache and memory pressure they did not cause, and the number comes out
# high with nothing in the result saying why.  2026-10-01 the throughput
# probe read the load once, BEFORE its first trial, and reported a 45%
# regression that was entirely a neighbour -- so this samples through
# the window and keeps the peak.
LOADS=""
( while :; do
    uptime | sed 's/.*averages: //' | awk '{print $1}'
    sleep 5
  done ) > /tmp/measure-idle-load.$$ 2>/dev/null &
LOAD_PID=$!
# Off the job table, or bash announces its own sampler's death on stderr
# and the announcement lands in whatever is reading the result.
disown "$LOAD_PID" 2>/dev/null || true
cleanup_load() { kill "$LOAD_PID" 2>/dev/null; rm -f "/tmp/measure-idle-load.$$"; }
trap cleanup_load EXIT

# Wall clock, not `time.monotonic()`: the system python on macOS counts
# that from its own start, so two separate calls both read ~0.004.
T0=$(python3 -c 'import time; print(f"{time.time():.3f}")')
sleep "$WINDOW"
T1=$(python3 -c 'import time; print(f"{time.time():.3f}")')
SECOND="$(sample)"
kill "$LOAD_PID" 2>/dev/null
wait "$LOAD_PID" 2>/dev/null
LOADS="$(tr '\n' ' ' < "/tmp/measure-idle-load.$$")"

ELAPSED=$(python3 -c "print(f'{$T1 - $T0:.3f}')")
# The window is measured, not assumed: a suspended laptop or a busy host
# can make `sleep 60` take minutes, and dividing by 60 would then
# under-report the cost by that factor.
python3 -c "import sys; sys.exit(0 if $ELAPSED >= $WINDOW * 0.95 else 1)" \
  || die "window was ${ELAPSED}s, expected ~${WINDOW}s -- the host was not idle enough to measure"

PIDS_BEFORE=$(echo "$FIRST" | awk '{print $1}' | sort | paste -sd, -)
PIDS_AFTER=$(echo "$SECOND" | awk '{print $1}' | sort | paste -sd, -)
[[ "$PIDS_BEFORE" == "$PIDS_AFTER" ]] \
  || die "the process set changed during the window (before: $PIDS_BEFORE / after: $PIDS_AFTER) -- the CPU delta would be meaningless"

OUT="$RESULTS_DIR/idle-${APP}-${SESSIONS}-${FOCUS}.json"
python3 - "$APP" "$SESSIONS" "$FOCUS" "$ELAPSED" "$OUT" "${IDLE_BUSY_LOAD:-5.0}" "$LOADS" <<PY
import json, sys
app, sessions, focus, elapsed, out = sys.argv[1], int(sys.argv[2]), sys.argv[3], float(sys.argv[4]), sys.argv[5]
busy_load = float(sys.argv[6])
loads = [float(x) for x in sys.argv[7].split()] if len(sys.argv) > 7 and sys.argv[7].strip() else []

def parse(block):
    rows = {}
    for line in block.strip().splitlines():
        pid, cpu, rss = line.split()
        parts = [float(x) for x in cpu.split(':')]
        secs = 0.0
        for p in parts:
            secs = secs * 60 + p
        rows[int(pid)] = (secs, int(rss))
    return rows

before = parse("""$FIRST""")
after = parse("""$SECOND""")
per = []
for pid in sorted(after):
    d = after[pid][0] - before[pid][0]
    # CPU time cannot run backwards; if it does, the pid was reused and
    # the sample is not of the same process.
    if d < 0:
        print(f"measure-idle: pid {pid} reported less CPU than before -- pid reuse", file=sys.stderr)
        raise SystemExit(2)
    per.append({"pid": pid, "cpu_secs": round(d, 2), "rss_kib": after[pid][1]})

total_cpu = round(sum(p["cpu_secs"] for p in per), 2)
total_rss = sum(p["rss_kib"] for p in per)
doc = {
    "app": app,
    "sessions": sessions,
    "focus": focus,
    "window_secs": round(elapsed, 3),
    "processes": len(per),
    "cpu_secs_total": total_cpu,
    "cpu_percent_of_one_core": round(100.0 * total_cpu / elapsed, 2),
    "rss_kib_total": total_rss,
    "rss_kib_per_session": round(total_rss / sessions, 1) if sessions else None,
    "host_load1_peak": max(loads) if loads else None,
    "host_load1_first": loads[0] if loads else None,
    "host_load1_last": loads[-1] if loads else None,
    "host_load1_samples": len(loads),
    "per_process": per,
}
with open(out, "w") as f:
    json.dump(doc, f, indent=1)
    f.write("\n")
print(f"    CPU {doc['cpu_percent_of_one_core']}% of one core over {doc['window_secs']}s"
      f"  |  RSS {total_rss // 1024} MiB total, {doc['rss_kib_per_session'] and round(doc['rss_kib_per_session']/1024, 1)} MiB per pane")
print(f"    {out}")
# An idle number needs an idle host.  The figure is written either way:
# it is still useful for an A/B on one machine.
if loads and max(loads) > busy_load:
    print(
        f"    NOT MEASURED: host load1 peaked at {max(loads)} during the window"
        f" (first {loads[0]}, last {loads[-1]}; busy over {busy_load}).  Idle cost"
        f" measured next to someone else's work is not idle cost.",
        file=sys.stderr,
    )
    raise SystemExit(3)
PY
rc=$?
# anything else the summary step left with is a crash of the instrument
case $rc in
  0|3) exit $rc ;;
  *) exit 2 ;;
esac
