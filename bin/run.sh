#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Dev sandbox: run.sh's marspot runs on its own MARSPOT_STATE_DIR,
# never the installed app's, so iterating here can't disturb the
# terminal you use.
source "$ROOT/bin/_dev-sandbox.sh"

PROFILE=debug
CARGO_FLAGS=()
for arg in "$@"; do
  case "$arg" in
    -r|--release) PROFILE=release; CARGO_FLAGS+=(--release) ;;
    -d|--debug)   PROFILE=debug ;;
    -h|--help)
      echo "usage: bin/run.sh [--release|--debug]"
      echo "  --release   build with optimizations (slower compile, faster runtime)"
      echo "  --debug     default; faster compile, slower runtime"
      exit 0
      ;;
    *)
      echo "unknown arg: $arg" >&2; exit 1 ;;
  esac
done

# Stop what a previous run.sh started in this sandbox -- shell, core and
# the sessions -- so the old window goes away during the build.  Only
# processes of this sandbox are touched (dev_kill_shell_core checks each
# pid's binary and state dir).
dev_kill_shell_core || true
rm -f "$MARSPOT_STATE_DIR/run.pid"

mkdir -p build
LOG="build/last-build.log"

echo "==> building marspot ($PROFILE)"
if ! cargo build ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"} > "$LOG" 2>&1; then
  echo
  echo "BUILD FAILED — last 60 lines of $LOG:"
  echo "----"
  tail -60 "$LOG"
  exit 1
fi

TARGET_DIR="$(cargo metadata --no-deps --format-version 1 2>/dev/null \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])' \
  2>/dev/null)"
TARGET_DIR="${TARGET_DIR:-target}"
BIN_DIR="$TARGET_DIR/$PROFILE"
for b in marspot-shell marspot-core marspot-session; do
  if [ ! -x "$BIN_DIR/$b" ]; then
    echo "error: binary not found at $BIN_DIR/$b" >&2
    exit 1
  fi
done
echo "==> launching $BIN_DIR/marspot-shell (core and sessions from $BIN_DIR)"
# The product's three processes: the shell owns the window and starts
# the core, the core starts one session per pane.  Fully detach stdio so
# the shell outlives this script; nohup also ignores SIGHUP.
MARSPOT_CORE_BIN="$BIN_DIR/marspot-core" MARSPOT_SESSION_BIN="$BIN_DIR/marspot-session" \
  nohup "$BIN_DIR/marspot-shell" > /dev/null 2>&1 < /dev/null &
pid=$!
disown
mkdir -p "$MARSPOT_STATE_DIR"
echo "$pid" > "$MARSPOT_STATE_DIR/run.pid"
dev_record_pid "$pid"

# Return once the shell is up, so callers chaining on run.sh don't race
# it.  Up to ~2s.
for _ in $(seq 1 40); do
  _dev_is_ours "$pid" && exit 0
  sleep 0.05
done
echo "warn: marspot-shell (pid $pid) is not running 2s after launch" >&2
exit 1
