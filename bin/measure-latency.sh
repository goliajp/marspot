#!/usr/bin/env bash
# bin/measure-latency.sh — where the time from a key press to the frame
# that shows it goes, in a sandbox of the shipped three processes.
#
# Opens real windows and is a measurement, so it runs on the runner,
# alone, in a build with the timing compiled in:
#   E2E_LOCK=bench E2E_FEATURES=latency-trace REMOTE_NAME=<own name> \
#     E2E_ENV="LAT_ROUNDS=4" bin/test-e2e-remote.sh measure-latency.sh
#
# One run is one sandbox app: it types LAT_KEYS keys into the first
# pane (src/app/key_script.rs), or lets a pane print for LAT_FLOOD_SECS,
# and bin/_latency_report.py reads the two trace files it leaves.
#
#   LAT_SIDES     "name=dir,name=dir" — directories holding the three
#                 binaries to compare; default "head=target/release".
#                 Runs are interleaved side by side within a round.
#   LAT_ROUNDS    rounds (default 4; the report drops each side's first)
#   LAT_SCENES    of: type-1,type-12,flood-1,paced-1,seen-1 (default the first three).
#                 seen: type-1 with the window captured after every key
#                 until the key shows (see src/app/key_script.rs)
#                 paced: a pane prints a line every 50 ms, so the frames
#                 per second are known without the timing compiled in
#                 and the counters give instructions per frame
#                 A scene may carry a window size in points and, for
#                 paced, the milliseconds between lines:
#                 paced-1@500x400@200
#   LAT_MEM       seconds: instead of timing, say what each process's
#                 memory is made of while the scene plays (`footprint`,
#                 `vmmap --summary`), then follow the shell's and the
#                 core's footprint for this long after output stops
#   LAT_EXTRA_WINDOWS  open this many more windows at start, each with
#                 a pane of its own that prints one screen and waits
#   LAT_KEYS      keys per typing run (default 200)
#   LAT_INTERVAL  ms between keys, plus up to a quarter (default 120)
#   LAT_FLOOD_SECS  seconds of output in a flood run (default 8)
#   LAT_OUT       where the per-run JSON goes (default $ROOT/build/latency)
#
# Bounded: rounds x sides x scenes runs, each at most LAT_RUN_MAX seconds
# (default 120), one app at a time, stopped through the sandbox helpers.
set -uo pipefail
trap 'exit 1' HUP INT TERM

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
dev_no_dock

SIDES="${LAT_SIDES:-head=$ROOT/target/release}"
ROUNDS="${LAT_ROUNDS:-4}"
SCENES="${LAT_SCENES:-type-1,type-12,flood-1}"
KEYS="${LAT_KEYS:-200}"
INTERVAL="${LAT_INTERVAL:-120}"
FLOOD_SECS="${LAT_FLOOD_SECS:-8}"
RUN_MAX="${LAT_RUN_MAX:-120}"
OUT="${LAT_OUT:-$ROOT/build/latency}"
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
PATTERN='/marspot(-shell|-core|-session)( |$)'

fail() {
  echo "FAIL: $*"
  tail -20 "$APPLOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}
trap dev_kill_shell_core EXIT

mkdir -p "$OUT"
[[ $(ps -o nice= -p $$ | tr -d ' ') == 0 ]] || fail "not at nice 0"
echo "==> $(sw_vers -productVersion) $(sw_vers -buildVersion), $(sysctl -n machdep.cpu.brand_string), load1 $(sysctl -n vm.loadavg | awk '{print $2}')"

# What a pane runs: a screen of text, then a prompt (or a flood first).
# inside a bench run every script a pane runs belongs in the run dir
PANE_SH="${MARSPOT_BENCH_RUN_DIR:-$MARSPOT_STATE_DIR}/lat-pane.sh"
mkdir -p "$MARSPOT_STATE_DIR" "$(dirname "$PANE_SH")"
FLOOD_FILE="$MARSPOT_STATE_DIR/lat-flood.txt"
python3 - "$FLOOD_FILE" <<'PY'
import sys
lines = []
for i in range(40000):
    lines.append(f"\x1b[3{i % 7 + 1}m{i:07d}\x1b[0m the quick brown fox jumps over the lazy dog 0123456789 {'=' * (i % 40)}")
open(sys.argv[1], "w").write("\n".join(lines) + "\n")
PY
cat > "$PANE_SH" <<EOF
#!/bin/bash
head -60 "$FLOOD_FILE"
# what this run's first pane does, written by the runner: "flood 8", "paced 8"
read -r mode secs gap < "$MARSPOT_STATE_DIR/lat-mode"
if [[ -n \$mode && ! -e "$MARSPOT_STATE_DIR/lat-flooded" ]]; then
  : > "$MARSPOT_STATE_DIR/lat-flooded"
  sleep 6
  if [[ \$mode == paced ]]; then
    python3 -c 'import sys, time
lines = open(sys.argv[1]).read().splitlines()
gap = int(sys.argv[3]) / 1000
for i in range(int(int(sys.argv[2]) / gap)):
    sys.stdout.write(lines[i] + "\n"); sys.stdout.flush(); time.sleep(gap)' "$FLOOD_FILE" "\$secs" "\${gap:-50}"
  else
    end=\$((SECONDS + secs))
    while (( SECONDS < end )); do cat "$FLOOD_FILE"; done
  fi
  : > "$MARSPOT_STATE_DIR/lat-flood-done"
fi
PS1='\$ ' exec /bin/sh
EOF
chmod +x "$PANE_SH"

write_layout() {
  python3 - "$MARSPOT_STATE_DIR" "$1" "$2" "$3" "$4" <<'PY'
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1]); cols, rows = int(sys.argv[2]), int(sys.argv[3])
def s(x): b = x.encode(); return struct.pack('<H', len(b)) + b
def pane(): return struct.pack('<Q', 0) + s('') + s('')
n = cols * rows
body  = struct.pack('<II', 0xA5505010, 2)
body += struct.pack('<HH', 0, 1)
body += struct.pack('<HHHH', cols, rows, 0, n) + b''.join(pane() for _ in range(n))
(d / 'shell-state.bin').write_bytes(body)
frames  = struct.pack('<IIH', 0xA5505011, 2, 1)
frames += struct.pack('<Idddd', 0, 40.0, 60.0, float(sys.argv[4]), float(sys.argv[5]))
(d / 'window-state.bin').write_bytes(frames)
PY
}

# one_run SIDE DIR SCENE ROUND
one_run() {
  local side=$1 dir=$2 scene=$3 round=$4 size gap
  IFS=@ read -r scene size gap <<<"$3"
  size=${size:-1000x760}
  local kind=${scene%-*} panes=${scene#*-} cols=1 rows=1
  [[ $panes == 12 ]] && { cols=4; rows=3; }
  local tag="$3.$side.r$round" prefix="$MARSPOT_STATE_DIR/lat"
  for b in marspot-shell marspot-core marspot-session; do
    [[ -x $dir/$b ]] || fail "$dir/$b is not built"
  done
  dev_kill_shell_core
  dev_wipe_state
  rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired" "$prefix".*.bin \
    "$MARSPOT_STATE_DIR/lat-flooded" "$MARSPOT_STATE_DIR/lat-flood-done"
  mkdir -p "$MARSPOT_STATE_DIR/logs"
  write_layout "$cols" "$rows" "${size%x*}" "${size#*x}" || fail "could not write the layout"

  local script=""
  case $kind in
    type) script="$KEYS,$INTERVAL,6000"; echo > "$MARSPOT_STATE_DIR/lat-mode" ;;
    seen) script="$KEYS,$INTERVAL,6000,capture"; echo > "$MARSPOT_STATE_DIR/lat-mode" ;;
    flood | paced) echo "$kind $FLOOD_SECS ${gap:-50}" > "$MARSPOT_STATE_DIR/lat-mode" ;;
    *) fail "no such scene: $scene" ;;
  esac
  env SHELL="$PANE_SH" MARSPOT_SESSION_BIN="$dir/marspot-session" MARSPOT_CORE_BIN="$dir/marspot-core" \
    MARSPOT_LATENCY_TRACE="$prefix" ${script:+MARSPOT_KEY_SCRIPT="$script"} \
    ${LAT_EXTRA_WINDOWS:+MARSPOT_DEV_EXTRA_WINDOWS="$LAT_EXTRA_WINDOWS"} \
    nohup "$dir/marspot-shell" >/tmp/marspot-measure-latency.log 2>&1 < /dev/null &
  local shell_pid=$!
  dev_record_pid "$shell_pid"
  disown
  # the shell's and the core's footprint over the first seconds: a
  # peak there is gone by the time the counters below are read
  "$ROOT/bin/_footprint_trace.py" "$shell_pid" --label "$tag" > "$MARSPOT_STATE_DIR/lat-footprint.txt" 2>&1 &
  local tracer=$!

  local i n=$((cols * rows))
  for (( i = 0; i < 300; i++ )); do
    (( $(dev_pane_shell_pids | grep -c .) >= n )) && break
    sleep 0.1
  done
  (( $(dev_pane_shell_pids | grep -c .) >= n )) || fail "$tag: panes did not come up"

  # CPU, instructions and wake-ups of every process while the scene plays
  local window
  if [[ -n $script ]]; then window=$(( KEYS * INTERVAL * 9 / 8000 - 2 )); else window=$(( FLOOD_SECS - 2 )); fi
  sleep 7
  if [[ -n ${LAT_MEM:-} ]]; then
    sleep 2   # output has been going for about three seconds
    local pid
    for pid in "$shell_pid" $(dev_core_pids | head -1); do
      echo "    -- $tag: $(ps -o comm= -p "$pid" | sed 's|.*/||') while presenting, footprint by category"
      footprint "$pid" 2>/dev/null | awk '/phys_footprint/ || ($1 ~ /^[0-9.]+$/ && ($2 == "MB" || $2 == "GB") && $1 >= 1)' | cut -c1-110 | sed 's/^/       /'
      echo "    -- vmmap regions of 2 MB resident or more"
      vmmap --summary "$pid" 2>/dev/null | awk '/^REGION TYPE/,/^TOTAL/' \
        | awk '$1 == "REGION" || /TOTAL/ || /[0-9.]+[MG] / ' | cut -c1-120 | sed 's/^/       /' | head -40
    done
  fi
  "$ROOT/bin/_idle_wakeups.py" --label "$tag" --window "$window" --readings 1 \
    --pattern "$dir$PATTERN" --json "$OUT/$tag.cpu.json" | sed 's/^/    /'

  local done_mark
  if [[ -n $script ]]; then
    for (( i = 0; i < RUN_MAX * 5; i++ )); do
      grep -q $'\tKEY_SCRIPT_DONE\t' "$APPLOG" 2>/dev/null && break
      sleep 0.2
    done
    grep -q $'\tKEY_SCRIPT_DONE\t' "$APPLOG" || fail "$tag: the key script did not finish"
  else
    for (( i = 0; i < RUN_MAX * 5; i++ )); do
      [[ -e $MARSPOT_STATE_DIR/lat-flood-done ]] && break
      sleep 0.2
    done
    [[ -e $MARSPOT_STATE_DIR/lat-flood-done ]] || fail "$tag: the flood did not finish"
  fi
  sleep 1.5   # both processes go idle and write out what they hold
  wait "$tracer"
  sed 's/^/    /' "$MARSPOT_STATE_DIR/lat-footprint.txt"
  if [[ -n ${LAT_MEM:-} ]]; then
    "$ROOT/bin/_footprint_trace.py" "$shell_pid" --label "$tag after output stopped" \
      --secs "$LAT_MEM" --every 0.5 --steps | sed 's/^/    /'
  fi
  local load
  load=$(sysctl -n vm.loadavg | awk '{print $2}')
  dev_kill_shell_core
  # a build without the timing leaves no trace: the counters above are its reading
  if compgen -G "$prefix.*.bin" >/dev/null; then
    "$ROOT/bin/_latency_report.py" "$prefix" --log "$APPLOG" --label "$tag load1=$load" --json "$OUT/$tag.json" \
      || fail "$tag: no report"
  else
    echo "== $tag load1=$load: no trace (built without latency-trace)"
  fi
}

for (( round = 1; round <= ROUNDS; round++ )); do
  for scene in ${SCENES//,/ }; do
    for pair in ${SIDES//,/ }; do
      one_run "${pair%%=*}" "${pair#*=}" "$scene" "$round"
    done
  done
done
echo "==> reports in $OUT"
