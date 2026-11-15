#!/usr/bin/env bash
# bin/test-frame-pairings.sh — a shell and a core of different builds
# still put every frame on the window.
#
# A shell that shows the core's surfaces directly has to be told by the
# core that it will wait for each frame to be reported shown; a core
# has to be told by the shell that the reports will come.  Either side
# can be the older build, and a running shell is handed a core of the
# other kind by every update and rollback.  For each of the four
# pairings of this build with the one in PAIR_OLD this starts a
# sandbox, lets a pane print in bursts, swaps the core for the other
# build and back while it prints, and checks after every step that
#
#   - the shell settled on the way this core's frames are shown
#     (FRAME_PACE: directly for this build's core, copied for the old),
#   - frames kept arriving (the count in SHOWN_CHECK grows), and
#   - what the window server composites is the surface last shown, the
#     right way up (SHOWN_CHECK, from a capture of the window).
#
# The checks are logged by this build's shell when it is built with the
# measuring compiled in, so with the old shell only its own log is read:
# the first present after each swap, and no error.
#
#   PAIR_OLD   directory holding the old marspot-shell, marspot-core
#              and marspot-session (required)
#   PAIR_NEW   this build's (default target/release; needs
#              --features latency-trace)
#
# Opens real windows: bin/test-e2e-remote.sh, never the interactive
# machine.  Bounded: four sandboxes one after another, each at most
# about a minute and a half, stopped through the sandbox helpers.
set -uo pipefail
trap 'exit 1' HUP INT TERM

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

NEW="${PAIR_NEW:-$ROOT/target/release}"
OLD="${PAIR_OLD:?PAIR_OLD: a directory with the old three binaries}"
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
TREE="$MARSPOT_STATE_DIR/binaries"
RUNLOG="$(dev_run_log test-frame-pairings)"

fail() {
  echo "FAIL: $*"
  tail -30 "$APPLOG" 2>/dev/null | cut -c1-400 | sed 's/^/    /'
  exit 1
}
trap dev_kill_shell_core EXIT

for d in "$NEW" "$OLD"; do
  for b in marspot-shell marspot-core marspot-session; do
    [[ -x $d/$b ]] || fail "$d/$b is not built"
    MARSPOT_NO_REDIRECT=1 "$d/$b" --version >/dev/null 2>&1
  done
done

# What the pane runs: a second of numbered lines, a second and a half of
# quiet, for as long as the sandbox lives.
PANE_SH="$MARSPOT_STATE_DIR/pairings-pane.sh"
mkdir -p "$MARSPOT_STATE_DIR"
cat > "$PANE_SH" <<'EOF'
#!/bin/bash
n=0
while :; do
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    n=$((n + 1))
    printf '\033[3%dm%06d\033[0m the quick brown fox jumps over the lazy dog %s\n' $((n % 7 + 1)) "$n" "$(printf '=%.0s' $(seq 1 $((n % 40 + 1))))"
    sleep 0.1
  done
  sleep 1.5
done
EOF
chmod +x "$PANE_SH"

count() { grep -c "$1" "$APPLOG" 2>/dev/null || true; }

wait_count() { # pattern, at least, seconds
  local i
  for (( i = 0; i < $3 * 5; i++ )); do
    (( $(count "$1") >= $2 )) && return 0
    sleep 0.2
  done
  return 1
}

field() { # line, name
  sed -n "s/.*[[:space:]]$2=\([^[:space:]]*\).*/\1/p" <<<"$1"
}

# The newest SHOWN_CHECK must be of a frame after `$2`, upright.
check_shown() { # step, frames before -> echoes frames now
  local step=$1 before=$2 line frames up flip i
  for (( i = 0; i < 60; i++ )); do
    line=$(grep $'\tSHOWN_CHECK\t' "$APPLOG" | tail -1)
    frames=$(field "$line" frames)
    [[ -n $frames ]] && (( frames > before )) && break
    sleep 0.2
  done
  [[ -n ${frames:-} ]] && (( frames > before )) || fail "$step: no frame was shown and checked after frame $before"
  up=$(field "$line" upright)
  flip=$(field "$line" flipped)
  awk -v u="$up" -v f="$flip" 'BEGIN { exit !(u >= 0.9 && u - f >= 0.2) }' \
    || fail "$step: the window does not show the surface (upright $up, upside down $flip)"
  echo "$frames"
}

swap_core() { # from which directory
  local swaps
  swaps=$(count $'\tUPDATE_SWAP\t')
  mkdir -p "$TREE/pending"
  cp "$1/marspot-core" "$TREE/pending/marspot-core"
  "$SHELL_BIN" --trigger >/dev/null
  wait_count $'\tUPDATE_SWAP\t' $((swaps + 1)) 90
}

# one_pairing SHELL_DIR CORE_DIR
one_pairing() {
  local shell_dir=$1 core_dir=$2 other name frames=0 step n expect
  if [[ $core_dir == "$NEW" ]]; then other=$OLD; else other=$NEW; fi
  name="shell $([[ $shell_dir == "$NEW" ]] && echo new || echo old), core $([[ $core_dir == "$NEW" ]] && echo new || echo old)"
  SHELL_BIN="$shell_dir/marspot-shell"
  dev_kill_shell_core
  dev_wipe_state
  mkdir -p "$MARSPOT_STATE_DIR/logs" "$TREE/current"
  cp "$core_dir/marspot-core" "$TREE/current/marspot-core"
  env SHELL="$PANE_SH" MARSPOT_SESSION_BIN="$shell_dir/marspot-session" \
    nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null &
  dev_record_pid $!
  disown

  n=0
  for step in "start on $core_dir" "swap to $other" "swap back to $core_dir"; do
    n=$((n + 1))
    local dir=${step##* }
    if (( n > 1 )); then
      swap_core "$dir" || fail "$name: $step: no UPDATE_SWAP"
    fi
    wait_count $'\tHELLO_ACK\t' "$n" 30 || fail "$name: $step: no HELLO_ACK"
    if [[ $shell_dir == "$NEW" ]]; then
      [[ $dir == "$NEW" ]] && expect=true || expect=false
      wait_count $'\tFRAME_PACE\t' "$n" 10 || fail "$name: $step: the shell did not settle how frames are shown"
      [[ $(field "$(grep $'\tFRAME_PACE\t' "$APPLOG" | tail -1)" paced) == "$expect" ]] \
        || fail "$name: $step: expected paced=$expect"
      # two bursts after the step, each checked
      frames=$(check_shown "$name: $step" "$frames") || { echo "$frames"; exit 1; }
      frames=$(check_shown "$name: $step, next burst" "$frames") || { echo "$frames"; exit 1; }
      [[ $(field "$(grep $'\tSHOWN_CHECK\t' "$APPLOG" | tail -1)" copied) == $([[ $expect == true ]] && echo false || echo true) ]] \
        || fail "$name: $step: frames reach the window the wrong way for this core"
    else
      # the old shell says only that a core's first frame went up
      wait_count $'\tWINDOW_FIRST_FRAME\t' "$n" 30 || fail "$name: $step: the core never painted"
      sleep 4
    fi
    grep -q $'\tERROR\t' "$APPLOG" && fail "$name: $step: an error was logged"
    kill -0 "$(dev_shell_pid)" 2>/dev/null || fail "$name: $step: the shell is gone"
    if [[ $shell_dir == "$NEW" ]]; then
      echo "  ok: $name: $step ($frames frames shown so far)"
    else
      echo "  ok: $name: $step"
    fi
  done
  dev_kill_shell_core
}

echo "==> $(sw_vers -productVersion) $(sw_vers -buildVersion), load1 $(sysctl -n vm.loadavg | awk '{print $2}')"
one_pairing "$NEW" "$NEW"
one_pairing "$NEW" "$OLD"
one_pairing "$OLD" "$NEW"
one_pairing "$OLD" "$OLD"
echo "ALL PASS"
