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

# Stop the instance a previous run.sh launched, so the old window
# disappears immediately instead of lingering through the build.  Only
# that one: its pid is in the sandbox, and it is checked to still be a
# marspot of this sandbox before anything is sent to it.
RUN_PID_FILE="$MARSPOT_STATE_DIR/run.pid"
if [[ -f $RUN_PID_FILE ]]; then
  prev=$(cat "$RUN_PID_FILE")
  if _dev_is_ours "$prev"; then
    echo "==> stopping the previous run.sh instance (pid $prev)"
    kill -TERM "$prev" 2>/dev/null || true
    for _ in $(seq 1 20); do
      kill -0 "$prev" 2>/dev/null || break
      sleep 0.1
    done
    kill -KILL "$prev" 2>/dev/null || true
  fi
  rm -f "$RUN_PID_FILE"
fi

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
BIN="$TARGET_DIR/$PROFILE/marspot"
if [ ! -x "$BIN" ]; then
  echo "error: binary not found at $BIN" >&2
  exit 1
fi
echo "==> launching $BIN"
# Fully detach stdio so the child outlives this script.  Without redirecting,
# closing the script's stdin/out/err can take marspot down with it on macOS.
# nohup additionally ignores SIGHUP for safety.
nohup "$BIN" > /dev/null 2>&1 < /dev/null &
pid=$!
disown
mkdir -p "$MARSPOT_STATE_DIR"
echo "$pid" > "$RUN_PID_FILE"
dev_record_pid "$pid"

# Return once the process is up, so callers chaining on run.sh don't
# race it.  Up to ~2s.
for _ in $(seq 1 40); do
  _dev_is_ours "$pid" && exit 0
  sleep 0.05
done
echo "warn: marspot (pid $pid) is not running 2s after launch" >&2
exit 1
