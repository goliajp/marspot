#!/usr/bin/env bash
# bin/drivers/mars.sh — automated mars launcher.
#
# Usage:
#   bin/drivers/mars.sh run-shell <shell-script-path> [extra-args...]
#     Launch mars with MARS_SHELL pointed at <shell-script-path>.  The
#     script becomes the shell for every spawned session — so when mars
#     boots into its 3×3 multi-session layout, all 9 sessions execute
#     <shell-script-path>.
#
# Why this driver exists: every other terminal needs AppleScript /
# keystroke gymnastics to spawn N parallel workloads; mars just needs
# its env-var contract.  Having a driver file keeps the scenario
# scripts uniform.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"

cmd=${1:-}
shift || true

case "$cmd" in
  run-shell)
    shell_script=${1:-}
    shift || true
    if [[ -z "$shell_script" || ! -x "$shell_script" ]]; then
      echo "mars.sh: shell script missing or not executable: $shell_script" >&2
      exit 2
    fi
    if [[ ! -x "$ROOT/target/release/mars" ]]; then
      ( cd "$ROOT" && cargo build --release 2>&1 | tail -3 ) >&2
    fi
    kill_app mars || true
    # Forward profiling env-vars so per-trial counters can be captured.
    env_pass=""
    for v in MARS_PROFILE MARS_LATENCY MARS_SCALE MARS_TMUX_DEBUG; do
      if [[ -n "${!v:-}" ]]; then env_pass+=" $v=${!v}"; fi
    done
    # nohup + redirect so the bench harness isn't tied to mars's output;
    # its result is reported through the marker file.
    (cd "$ROOT" && env $env_pass MARS_SHELL="$shell_script" \
      nohup target/release/mars "$@" > /dev/null 2>&1 < /dev/null &) || true
    disown 2>/dev/null || true
    # Don't `wait` — mars exits when all sessions exit, scenario script polls marker.
    ;;
  *)
    echo "mars.sh: unknown sub-command: ${cmd:-(none)}" >&2
    echo "  usage: mars.sh run-shell <shell-script-path> [extra-args...]" >&2
    exit 2
    ;;
esac
