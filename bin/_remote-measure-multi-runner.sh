#!/usr/bin/env bash
# bin/_remote-measure-multi-runner.sh — N terminal windows absorbing the
# same corpus at the same moment, for the product and for the terminals
# whose drivers can open N windows on the bench host.  Runs there, the
# way bin/_remote-measure-others-runner.sh does, and prints one JSON
# object on stdout:
#
#   { "windows": 4, "host": {...},
#     "marspot": { "grid": "99x35", "cat-ascii_MBps": 301.2, ..., "rounds": [...] },
#     "ghostty": { ... } }
#
#   MULTI_WINDOWS=4 MEASURE_ROUNDS=4 bash bin/_remote-measure-multi-runner.sh
#
# What a figure is.  Every window runs the same trial script.  A trial
# has a start time all N windows share, read off the wall clock; each
# window `cat`s the corpus, moves the cursor to the sentinel position
# and asks where the cursor is, as the single-window measurement does.
# The figure is the bytes of all N windows over the time from the
# earliest start to the latest certified end.  A trial counts only when
# every window started within START_SKEW_S of the shared start and
# every window's reply carried the sentinel position.
#
# The product is one marspot-shell with N windows of one pane each;
# Ghostty is N `ghostty -e` processes of one window each.  Both get the
# same grid (the product's window is sized to what Ghostty opens with).
#
# Nothing is stopped by name: the product by the pids its own state dir
# recorded, Ghostty by the pids this run was given when it started them.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
source "$ROOT/bin/_lib.sh"

# shellcheck disable=SC2206
SCENARIOS=(${MULTI_SCENARIOS:-cat-ascii cat-mixed cat-cjk cat-emoji})
# shellcheck disable=SC2206
TERMS=(${MEASURE_TERMS:-ghostty marspot})
N=${MULTI_WINDOWS:?MULTI_WINDOWS=<how many windows>}
ROUNDS=${MEASURE_ROUNDS:-4}
TRIALS=${MULTI_TRIALS:-3}
# Seconds from one trial's start to the next.  Longer than the slowest
# trial, or the next one starts late and does not count.
SLOT_S=${MULTI_SLOT_S:-4}
START_SKEW_S=0.03
COOLDOWN_S=3
[[ $N =~ ^[1-9][0-9]?$ ]] || { echo "MULTI_WINDOWS wants a count, got '$N'" >&2; exit 2; }

trap 'exit 143' TERM
trap 'exit 130' INT
trap 'exit 129' HUP

SESSION_DIR="/tmp/marspot-multi-$$"
mkdir -p "$SESSION_DIR"
chmod 0755 "$SESSION_DIR"
RUN_STARTED=$(date +%s)
GHOSTTY_PIDS="$SESSION_DIR/ghostty.pids"

load1() { sysctl -n vm.loadavg | awk '{print $2}'; }

# Stop the Ghostty processes this run started that are still there:
# only pids it was given, only if the process behind the pid is a
# Ghostty that began after this run did.
stop_ghostty() {
  [[ -f $GHOSTTY_PIDS ]] || return 0
  local pid comm began began_s
  while read -r pid; do
    [[ $pid =~ ^[0-9]+$ ]] && (( pid > 300 )) || continue
    comm="$(ps -o comm= -p "$pid" 2>/dev/null || true)"
    [[ $comm == */Ghostty.app/Contents/MacOS/ghostty ]] || continue
    began="$(ps -o lstart= -p "$pid" 2>/dev/null || true)"
    began_s="$(date -j -f '%a %b %d %T %Y' "$began" +%s 2>/dev/null || echo 0)"
    (( began_s >= RUN_STARTED )) || continue
    kill -TERM "$pid" 2>/dev/null || true
  done < "$GHOSTTY_PIDS"
  : > "$GHOSTTY_PIDS"
}

cleanup() {
  local rc=$?
  trap - EXIT
  set +e
  stop_ghostty
  bin/drivers/marspot.sh close-app >/dev/null 2>&1
  [[ -n ${MULTI_KEEP:-} ]] || rm -rf "$SESSION_DIR"
  bench_run_end
  exit "$rc"
}
trap cleanup EXIT
bench_run_begin   # the product runs from the run dir's copies
# and so do the scripts its panes run; the markers stay in SESSION_DIR
SCRIPT_DIR="$MARSPOT_BENCH_RUN_DIR/multi"
mkdir -p "$SCRIPT_DIR"

# The script every window of one terminal and round runs.
write_multi_script() {
  local out=$1 prefix=$2 start_file=$3
  {
    echo '#!/bin/zsh'
    echo 'zmodload zsh/datetime'
    # a pane whose shell has exited may be given a new one: the round
    # is over by then, and that one measures nothing
    echo "[[ ! -e \"$start_file.end\" ]] || exec sleep 20"
    echo "marker=\"$prefix.\$\$\""
    echo 'echo "==GRID== $(stty size < /dev/tty)" >> "$marker"'
    # the first start, written once every window is up
    echo "until [[ -s \"$start_file\" ]]; do sleep 0.05; done"
    echo "t0=\$(<\"$start_file\")"
    local scenario reps args t k=0
    for scenario in "${SCENARIOS[@]}"; do
      reps=$(live_repeat "$ROOT/bench/scenarios/$scenario.bin")
      args=$(live_cat_args "$ROOT/bench/scenarios/$scenario.bin" "$reps")
      for ((t = 1; t <= TRIALS; t++)); do
        echo "echo '==SCN== $scenario' >> \"\$marker\""
        echo "__at=\$((t0 + $k * $SLOT_S))"
        # asleep until just before the start, then on the clock
        echo '__w=$((__at - EPOCHREALTIME - 0.02)); (( __w > 0 )) && sleep $__w'
        echo 'while (( EPOCHREALTIME < __at )); do :; done'
        echo '__s=$EPOCHREALTIME'
        echo "{ /bin/zsh -c '"
        echo "  /bin/cat$args"
        echo '  stty raw -echo 2>/dev/null'
        echo '  printf "\033[7;11H\033[6n" > /dev/tty'
        echo '  IFS="[;" read -t 5 -srd R _ _row _col < /dev/tty'
        echo '  stty sane 2>/dev/null'
        echo '  printf "cpr=%s,%s\n" "$_row" "$_col" >&2'
        echo "'; } 2>> \"\$marker\""
        echo '__e=$EPOCHREALTIME'
        echo "printf 'span %.6f %.6f %.6f\n' \$__at \$__s \$__e >> \"\$marker\""
        k=$((k + 1))
      done
    done
    echo "echo '==ALL_DONE==' >> \"\$marker\""
    # the window stays until its terminal's CPU time has been read
    echo "until [[ -e \"$start_file.end\" ]]; do sleep 0.05; done"
  } > "$out"
  chmod 0755 "$out"
}

# CPU seconds, user and system, the processes of one terminal have used
# so far: the product's shell, core and sessions, or the Ghostty
# processes this run started.
cpu_s() {
  local pids
  case "$1" in
    marspot) pids="$(bin/drivers/marspot.sh app-pids 2>/dev/null | tr '\n' ',')" ;;
    ghostty) pids="$(tr '\n' ',' < "$GHOSTTY_PIDS")" ;;
  esac
  pids="${pids%,}"
  [[ -n $pids ]] || { echo 0; return; }
  ps -o time= -p "$pids" 2>/dev/null | awk '{ n = split($1, t, ":"); s = 0; for (i = 1; i <= n; i++) s = s * 60 + t[i]; sum += s } END { printf "%.2f\n", sum }'
}

# markers of one terminal and round that contain a line
count_with() {
  local n=0 f
  for f in "$1".*; do
    [[ -f $f ]] && grep -q "$2" "$f" && n=$((n + 1))
  done
  echo "$n"
}

for s in "${SCENARIOS[@]}"; do
  [[ -f bench/scenarios/$s.bin ]] || { echo "missing bench/scenarios/$s.bin" >&2; exit 1; }
done
SLOTS=$(( ${#SCENARIOS[@]} * TRIALS ))

for ((i = 0; i < 18; i++)); do
  awk -v l="$(load1)" 'BEGIN { exit !(l > 2.5) }' || break
  sleep 10
done
echo "==> $N windows, load1 $(load1) before the first round" >&2

for ((round = 1; round <= ROUNDS; round++)); do
for t in "${TERMS[@]}"; do
  prefix="$SESSION_DIR/m-$t-r$round"
  start_file="$SESSION_DIR/start-$t-r$round"
  script="$SCRIPT_DIR/trial-$t-r$round.sh"
  write_multi_script "$script" "$prefix" "$start_file"
  wrapper="$SCRIPT_DIR/wrapper-$t-r$round"
  printf '#!/bin/bash\n%s; exit\n' "$script" > "$wrapper"
  chmod 0755 "$wrapper"
  load_start="$(load1)"
  echo "==> [$t r$round] launch $N (load1 $load_start)" >&2
  case "$t" in
    marspot)
      bin/drivers/marspot.sh run-app "$wrapper" "$N" >&2
      ;;
    ghostty)
      for ((i = 0; i < N; i++)); do
        bin/drivers/ghostty.sh run-recorded "$GHOSTTY_PIDS" "$wrapper" >&2
      done
      ;;
    *) echo "no way to open $N windows of $t" >&2; exit 2 ;;
  esac

  up=0
  for ((i = 0; i < 300; i++)); do
    up=$(count_with "$prefix" '==GRID==')
    (( up >= N )) && break
    sleep 0.2
  done
  if (( up < N )); then
    echo "==> [$t r$round] only $up of $N windows came up; no reading" >&2
  else
    cpu_before="$(cpu_s "$t")"
    python3 -c 'import time; print(f"{time.time() + 2:.6f}")' > "$start_file.tmp"
    mv "$start_file.tmp" "$start_file"
    for ((i = 0; i < SLOTS * SLOT_S + 30; i++)); do
      (( $(count_with "$prefix" '==ALL_DONE==') >= N )) && break
      sleep 1
    done
    echo "$cpu_before $(cpu_s "$t")" > "$prefix.cpu"
  fi
  echo "$load_start $(load1)" > "$prefix.load"
  # the windows may go now
  : > "$start_file.end"
  sleep 1

  case "$t" in
    marspot) bin/drivers/marspot.sh close-app >&2 || true ;;
    ghostty) sleep 1; stop_ghostty ;;
  esac
  sleep "$COOLDOWN_S"
done
done

LIVE_MIN_BYTES="$LIVE_MIN_BYTES" LIVE_SENTINEL_CPR="$LIVE_SENTINEL_CPR" \
  python3 "$ROOT/bin/_multi_aggregate.py" "$SESSION_DIR" "$N" "$ROUNDS" "$TRIALS" "$START_SKEW_S" \
    "${TERMS[*]}" "${SCENARIOS[@]}"
