#!/usr/bin/env bash
# Shared helpers for bench scripts.  Source this; do not execute.
#
# Conventions every script that sources this should follow:
#  - $ROOT points at marspot repo root (set by source path resolution below)
#  - $SCENARIOS_DIR / $RESULTS_DIR exist
#  - All marker files go under /tmp with a known prefix so we can clean
#    them between trials without fighting old runs
#  - Runner hosts come from marspot_host, never from a literal name

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

# ---- runner hosts ----------------------------------------------------------
# Tests, the CI mac job and window e2e go to the test host.  Benchmarks go
# to the bench host, which runs nothing else: there the build and the
# measurement share one `bench-lock bench` hold.  The environment overrides
# either name; how a host is picked changes here and nowhere else.
MARSPOT_TEST_HOST="${MARSPOT_TEST_HOST:-mini2}"
MARSPOT_BENCH_HOST="${MARSPOT_BENCH_HOST:-mini3}"
marspot_host() {
  case "${1:-}" in
    test)  echo "$MARSPOT_TEST_HOST" ;;
    bench) echo "$MARSPOT_BENCH_HOST" ;;
    *) echo "marspot_host: test|bench, not '${1:-}'" >&2; return 2 ;;
  esac
}

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
# marspot_bin <name> [profile=release] → absolute path of built binary;
# inside a bench run, the run's copy of a release binary
marspot_bin() {
  local name=$1
  local profile=${2:-release}
  if [[ $profile == release && -n "${MARSPOT_BENCH_BIN_DIR:-}" ]]; then
    echo "$MARSPOT_BENCH_BIN_DIR/$name"
  else
    echo "$(_mars_target_dir)/$profile/$name"
  fi
}

# ---- the bench run dir -------------------------------------------------------
# Every executable a measurement launches is a copy under
# $MARSPOT_BENCH_RUN_ROOT/<run-id>/: the bench host's console guard exempts
# that prefix alone, and only while `bench-lock bench` is held.  State dirs
# stay in /tmp.  The outermost script calls bench_run_begin after the build
# and bench_run_end on every exit; the scripts it runs inherit
# MARSPOT_BENCH_RUN_DIR and MARSPOT_BENCH_BIN_DIR, launch only those copies,
# build nothing and leave the dir alone.  The build has to make every
# release binary and the l3_throughput example first: what is missing from
# the target dir is missing from the run.
MARSPOT_BENCH_RUN_ROOT="${MARSPOT_BENCH_RUN_ROOT:-$HOME/work/bench-marspot/run}"
_BENCH_RUN_OWNED=""
bench_run_begin() {
  [[ -z "${MARSPOT_BENCH_RUN_DIR:-}" ]] || return 0
  local src f
  src="$(_mars_target_dir)/release"
  MARSPOT_BENCH_RUN_DIR="$MARSPOT_BENCH_RUN_ROOT/$(date -u +%Y%m%dT%H%M%SZ)-$$"
  MARSPOT_BENCH_BIN_DIR="$MARSPOT_BENCH_RUN_DIR/bin"
  bench_run_prune
  mkdir -p "$MARSPOT_BENCH_BIN_DIR/examples"
  _BENCH_RUN_OWNED=1
  # cp, never ln: a link would launch the file in the target dir
  for f in "$src"/* "$src/examples/l3_throughput"; do
    [[ -f $f && -x $f ]] || continue
    cp "$f" "$MARSPOT_BENCH_BIN_DIR/${f#"$src"/}"
  done
  export MARSPOT_BENCH_RUN_DIR MARSPOT_BENCH_BIN_DIR
}
# A run stopped by SIGKILL never reaches its EXIT trap and leaves its dir.
# Only a run that holds `bench-lock bench` (MARSPOT_BENCH_LOCKED=1, set
# inside the lock by whoever took it) can know no other run is using a
# sibling dir, so only then are the ones older than this run removed.
bench_run_prune() {
  [[ ${MARSPOT_BENCH_LOCKED:-} == 1 && -d $MARSPOT_BENCH_RUN_ROOT ]] || return 0
  local d mine
  mine="$(basename "$MARSPOT_BENCH_RUN_DIR")"
  for d in "$MARSPOT_BENCH_RUN_ROOT"/*; do
    [[ -d $d && ! -L $d ]] || continue
    # run ids are UTC stamps first, so name order is age order
    [[ "$(basename "$d")" < "$mine" ]] || continue
    echo "==> removing a run dir an earlier run left: $d"
    rm -rf "${d:?}"
  done
}
bench_run_end() {
  [[ -n "$_BENCH_RUN_OWNED" ]] || return 0
  _BENCH_RUN_OWNED=""
  rm -rf "${MARSPOT_BENCH_RUN_DIR:?}"
}

# bench_settle <seconds> — let the machine come down off the build before
# measuring anything; 0 opts out.
#
# The per-scenario warm-up trial already pays the cold-CACHE cost, which
# is a different thing: a `cargo build --release` with fat LTO pins every
# core for minutes, and the cores are still hot (and clocked down) the
# instant it exits.  Measured 2026-08-19: headless parse inside `--full`
# read cjk 187.2 / emoji 126.6, while the same binary re-measured by hand
# a few minutes later read 215.7 / 140.7 — 13-15 % apart, enough to fail
# floors that nothing had regressed past.
#
# A fixed wait is not enough: a fat-LTO build leaves load high for longer
# than 8 s, and the first scenario is measured right into it (four runs on
# one commit read 185.5 / 202.8 / 203.3 / 207.6 MB/s on cat-cjk with a
# build, then 229.5 / 235.3 / 238.2 when the build was a no-op).  So it
# also waits for load1 under 3, up to four minutes: the load average is a
# one-minute mean, and from the 8 a nine-session trial leaves behind it
# takes about a minute and a half to fall under 3.
bench_settle() {
  local settle=$1 waited=0 l1=""
  [[ "$settle" != "0" ]] || return 0
  echo "==> settling ${settle}s after build"
  sleep "$settle"
  while [[ "$waited" -lt 240 ]]; do
    l1="$(sysctl -n vm.loadavg 2>/dev/null | awk '{print $2}')"
    [[ -n "$l1" ]] || return 0
    if awk -v l="$l1" 'BEGIN { exit !(l < 3.0) }'; then
      [[ "$waited" -eq 0 ]] || echo "    load down to $l1 after ${waited}s"
      return 0
    fi
    sleep 3
    waited=$((waited + 3))
  done
  echo "    still warm after ${waited}s (load1=$l1) — measuring anyway"
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
# marspot: shell, core and sessions of the bench sandbox that
# `bin/drivers/marspot.sh run-app` started (its own state dir, so never
# anyone else's marspot).  The other terminals are whole apps the bench
# drives, matched by name.
# macOS quirks:
#   - pgrep -x doesn't match because comm is the full path on macOS,
#     not the basename — `pgrep -x iTerm2` returns nothing even when
#     iTerm2 is running.
#   - pgrep -fl matches the full command line; we filter the result.
# Returns empty if nothing matches.  Don't depend on order.
pids_of() {
  # Process basename map:
  #   iterm    → iTerm2    (iTerm2 main process)
  #   warp     → stable    (Warp's binary name; verified via ps -axo comm)
  #   terminal → Terminal  (Terminal.app)
  case "$1" in
    marspot)  "$ROOT/bin/drivers/marspot.sh" app-pids 2>/dev/null || true ;;
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

# kill_app <name>
# marspot: stops the bench sandbox `bin/drivers/marspot.sh run-app`
# started and removes its state dir; no other marspot is touched.  For
# iterm2 / warp / terminal.app, use close_bench_windows below to close
# ONLY the windows we opened, leaving the user's work alone.
kill_app() {
  case "$1" in
    marspot)
      "$ROOT/bin/drivers/marspot.sh" close-app >/dev/null 2>&1 || true
      ;;
    *)
      echo "kill_app: refusing to quit '$1' — bench must not destroy user state." >&2
      echo "          Use close_bench_windows() to clean up only what we opened." >&2
      return 2
      ;;
  esac
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
  # to front for the scenarios that drive keystrokes.
  case "$app" in
    iTerm2|iTerm|Terminal|Warp|stable|marspot|marspot-shell) return 0 ;;
  esac
  osascript -e "tell application \"System Events\" to set frontmost of first process whose name is \"$app\" to true" \
    >/dev/null 2>&1 || true
}

# LIVE_MIN_BYTES and the live trial script every terminal is handed
# shellcheck source=bin/_lib-live.sh
source "$ROOT/bin/_lib-live.sh"
