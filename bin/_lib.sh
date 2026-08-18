#!/usr/bin/env bash
# Shared helpers for bench scripts.  Source this; do not execute.
#
# Conventions every script that sources this should follow:
#  - $ROOT points at marspot repo root (set by source path resolution below)
#  - $SCENARIOS_DIR / $RESULTS_DIR exist
#  - All marker files go under /tmp with a known prefix so we can clean
#    them between trials without fighting old runs
#
# The whole bench mechanism is documented in docs/bench.md.  Read that
# before adding to this file.

# Resolve $ROOT to the marspot repo root from wherever we're sourced.
# This works whether the caller is bin/foo.sh or bin/scenarios/bar.sh.
if [[ -z "${ROOT:-}" ]]; then
  _self="${BASH_SOURCE[1]:-${BASH_SOURCE[0]}}"
  ROOT="$(cd "$(dirname "$_self")/.." 2>/dev/null && pwd)"
  # If we're under bin/scenarios/ or bin/drivers/, go up one more.
  if [[ ! -d "$ROOT/bench" && -d "$(dirname "$ROOT")/bench" ]]; then
    ROOT="$(dirname "$ROOT")"
  fi
fi
SCENARIOS_DIR="$ROOT/bench/scenarios"
RESULTS_DIR="$ROOT/bench/results"
mkdir -p "$RESULTS_DIR"

MARKER_PREFIX="/tmp/marspot-bench"

# ---- cargo binary resolution -------------------------------------------
#
# On systems with a global CARGO_TARGET_DIR (e.g. external SSD per
# cargo-target-dir.md), `$ROOT/target/release/<bin>` does NOT exist.
# Resolve target_directory via `cargo metadata` once and cache it.
# Callers should use `marspot_bin <bin>` instead of hardcoding paths.
_MARSPOT_TARGET_DIR=""
_mars_target_dir() {
  if [[ -z "$_MARSPOT_TARGET_DIR" ]]; then
    _MARSPOT_TARGET_DIR=$(cd "$ROOT" && cargo metadata --no-deps --format-version 1 2>/dev/null \
      | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])' 2>/dev/null)
    [[ -z "$_MARSPOT_TARGET_DIR" ]] && _MARSPOT_TARGET_DIR="$ROOT/target"
  fi
  echo "$_MARSPOT_TARGET_DIR"
}
# marspot_bin <name> [profile=release] → absolute path of built binary
marspot_bin() {
  local name=$1
  local profile=${2:-release}
  echo "$(_mars_target_dir)/$profile/$name"
}

# ---- timing parser -------------------------------------------------------
#
# `/usr/bin/time -p` writes POSIX format (`real 1.234`); zsh/bash builtin
# `time` writes `real 0m1.234s`.  Handle both — different terminals'
# default shells differ, and we don't want to depend on which one ran.
parse_real_ns() {
  local marker=$1
  awk '
    /^real/ {
      s = $2
      if (s ~ /m/) {
        split(s, parts, "m")
        mins = parts[1]+0
        sub("s", "", parts[2])
        secs = parts[2]+0
        total = mins*60 + secs
      } else {
        total = s+0
      }
      printf "%d\n", total * 1e9
    }
  ' "$marker"
}

# parse_all_real_ns marker → emits one ns per `real` line (multi-trial markers)
parse_all_real_ns() { parse_real_ns "$@"; }

# wait_for_marker <marker> [timeout_s=180] [sentinel=ALL_DONE]
# Returns 0 when the marker exists and contains the sentinel; 1 on timeout.
wait_for_marker() {
  local marker=$1
  local timeout_s=${2:-180}
  local sentinel=${3:-==ALL_DONE==}
  local deadline=$(( $(date +%s) + timeout_s ))
  while [[ $(date +%s) -lt $deadline ]]; do
    if [[ -s "$marker" ]] && grep -q "$sentinel" "$marker" 2>/dev/null; then
      return 0
    fi
    sleep 0.5
  done
  return 1
}

# wait_for_n_lines <marker> <pattern> <n> [timeout_s=180]
# Used for parallel scenarios where N independent workers each emit a
# match.  Returns 0 once $n matching lines are seen.
wait_for_n_lines() {
  local marker=$1
  local pattern=$2
  local n=$3
  local timeout_s=${4:-180}
  local deadline=$(( $(date +%s) + timeout_s ))
  while [[ $(date +%s) -lt $deadline ]]; do
    if [[ -s "$marker" ]]; then
      local count
      count=$(grep -c "$pattern" "$marker" 2>/dev/null || echo 0)
      if (( count >= n )); then
        return 0
      fi
    fi
    sleep 0.5
  done
  return 1
}

# median ns ns ns... → median ns on stdout (integer, sorted)
median() {
  python3 -c '
import sys
xs = sorted(int(x) for x in sys.argv[1:] if x.strip())
print(xs[len(xs)//2] if xs else 0)
' "$@"
}

# rss_kib <pid> → current RSS in KiB, or empty on failure
rss_kib() {
  ps -o rss= -p "$1" 2>/dev/null | tr -d ' '
}

# pids_of <terminal-name> → newline-separated list of matching pids.
# macOS quirks:
#   - pgrep -x doesn't match because comm is the full path on macOS,
#     not the basename — `pgrep -x iTerm2` returns nothing even when
#     iTerm2 is running.
#   - pgrep -fl matches the full command line; we filter the result.
# Returns empty if nothing matches.  Don't depend on order.
pids_of() {
  # macOS pgrep matches against `comm` (the executable basename) by
  # default — that's the most reliable mode and what we use here.
  # `-f` (match full cmdline) gives inconsistent results across
  # macOS versions for paths under `/Applications`, so we avoid it.
  # Process basename map:
  #   marspot     → marspot      (our binary)
  #   mcli     → mcli      (our binary)
  #   iterm    → iTerm2    (iTerm2 main process)
  #   warp     → stable    (Warp's binary name; verified via ps -axo comm)
  #   terminal → Terminal  (Terminal.app)
  case "$1" in
    marspot)     pgrep -al '^marspot$'     2>/dev/null | awk '{print $1}' ;;
    mcli)     pgrep -al '^mcli$'     2>/dev/null | awk '{print $1}' ;;
    iterm)    pgrep -al '^iTerm2$'   2>/dev/null | awk '{print $1}' ;;
    warp)     pgrep -al '^stable$'   2>/dev/null | awk '{print $1}' ;;
    terminal) pgrep -al '^Terminal$' 2>/dev/null | awk '{print $1}' ;;
  esac
}

# rss_total_kib <terminal-name> → sum of RSS (KiB) across all matching pids.
# Empty if no process is running.  Used to attribute multi-session RSS;
# delta vs. baseline tells us how much the bench scenario added.
rss_total_kib() {
  local pids; pids=$(pids_of "$1")
  [[ -z "$pids" ]] && return 0
  local sum=0
  for p in $pids; do
    local r; r=$(ps -o rss= -p "$p" 2>/dev/null | tr -d ' ')
    [[ -n "$r" ]] && sum=$(( sum + r ))
  done
  echo "$sum"
}

# json_emit_kv <key> <value> [more...] → prints `"k":"v","k":v,...`
# Heuristic: numeric values pass-through unquoted; everything else quoted.
# Don't use this for nested objects — write JSON directly via python3.
json_emit_kv() {
  local out=""
  while (( $# >= 2 )); do
    local k=$1; local v=$2; shift 2
    if [[ -n "$out" ]]; then out+=","; fi
    if [[ "$v" =~ ^-?[0-9]+(\.[0-9]+)?$ ]] || [[ "$v" == "true" || "$v" == "false" || "$v" == "null" ]]; then
      out+="\"$k\":$v"
    else
      out+="\"$k\":\"$v\""
    fi
  done
  printf '%s' "$out"
}

# kill_app <name>
# DANGEROUS — quits the named terminal app entirely, including the
# user's open windows.  Only safe for `marspot` (our SUT) and `mcli`.
# For iterm2 / warp / terminal.app, use close_bench_windows below
# to close ONLY the windows we opened, leaving the user's work alone.
kill_app() {
  case "$1" in
    marspot|mcli)
      # marspot/mcli are not the user's daily driver in this repo.
      # Match by the release/debug binary path so we don't hit any
      # `marspot` script the user might have on PATH.
      local pids; pids=$(pids_of "$1")
      for p in $pids; do kill "$p" 2>/dev/null || true; done
      sleep 0.3
      ;;
    *)
      echo "kill_app: refusing to quit '$1' — bench must not destroy user state." >&2
      echo "          Use close_bench_windows() to clean up only what we opened." >&2
      return 2
      ;;
  esac
}

# Pretty-print a number of bytes as MiB with one decimal.
fmt_mib() { python3 -c "print(f'{$1/1048576:.1f}')"; }
fmt_mbps_from_ns() {
  # bytes ns → MiB/s
  python3 -c "print(f'{$1*1e9/$2/1048576:.1f}')"
}

# ---- focus isolation ---------------------------------------------------
#
# Bench windows must NOT steal focus from the user's foreground app —
# popping iTerm to the front during a run blocks anyone trying to do
# other work.  These helpers capture the user's current frontmost
# process before we open bench windows, and restore focus to it
# afterwards.  The bench windows still exist behind the user's window
# (so they keep rendering / draining PTY), they just don't grab the
# user's attention.

# Returns the name of the currently-frontmost macOS process, or empty.
# Use with caution: it returns the app's "process name" which usually
# matches the value `tell application "<x>" to activate` accepts.
current_frontmost_app() {
  osascript -e 'tell application "System Events" to return name of first process whose frontmost is true' 2>/dev/null
}

# Re-activate the named app.  Best-effort.  Empty arg → no-op.
# Uses System Events `set frontmost` because some apps (notably
# WeChat, certain Electron apps) don't respond to direct AppleScript
# `tell application X to activate`.
restore_focus_to() {
  local app=$1
  [[ -z "$app" ]] && return 0
  # Skip if the app is one of our bench targets — those need to come
  # to front for typing-latency / scenarios that drive keystrokes.
  case "$app" in
    iTerm2|iTerm|Terminal|Warp|stable|marspot|mcli) return 0 ;;
  esac
  osascript -e "tell application \"System Events\" to set frontmost of first process whose name is \"$app\" to true" \
    >/dev/null 2>&1 || true
}

# ---- live measurement sizing --------------------------------------------
# Minimum bytes a LIVE trial must push through a terminal.
#
# The `time cat` trick only measures a terminal once `cat` starts
# blocking on PTY writes.  Below that the kernel buffer swallows the
# file and `cat` returns having measured nothing — a 64 KiB scenario
# reports 0 ms on every terminal — while a few-MiB one spends most of
# its window in the warm-up region.  Measured 2026-08-18 on the emoji
# corpus: one binary reported 56 MB/s at 8 MiB and 140 MB/s at 67 MiB.
# A pipeline cannot have two throughputs; the small sample was lying,
# and it lies WORSE the faster the terminal is — the wrong bias for
# this repo.
#
# Every live consumer (bin/measure.sh for marspot, bin/measure-other.sh
# for the competitors) cat's each scenario as many times as it takes to
# clear this bar, so both sides of a cross-terminal comparison push the
# same number of bytes.  The scenario FILES keep their original sizes:
# `--bench parse` feeds bytes directly with no PTY in the loop, so the
# headless floors in bench/baseline.json stay comparable across this
# change.
LIVE_MIN_BYTES=$((32 * 1024 * 1024))

# Repeat count for one scenario file to clear LIVE_MIN_BYTES.
live_repeat() {
  local size
  size=$(stat -f%z "$1")
  local n=$(( (LIVE_MIN_BYTES + size - 1) / size ))
  [[ "$n" -lt 1 ]] && n=1
  echo "$n"
}

# The repeated argument list: "<path> <path> ..." n times.
live_cat_args() {
  local path=$1 n=$2 out="" i=1
  while [[ $i -le $n ]]; do out="$out $path"; i=$((i + 1)); done
  echo "$out"
}
