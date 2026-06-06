#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

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

# Kill any previously-running instance up-front so the old window disappears
# immediately, instead of lingering through the build.
if pgrep -x mars > /dev/null; then
  echo "==> killing previous mars instance(s)"
  killall mars 2>/dev/null || true
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    pgrep -x mars > /dev/null || break
    sleep 0.1
  done
fi

mkdir -p build
LOG="build/last-build.log"

echo "==> building mars ($PROFILE)"
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
BIN="$TARGET_DIR/$PROFILE/mars"
if [ ! -x "$BIN" ]; then
  echo "error: binary not found at $BIN" >&2
  exit 1
fi
echo "==> launching $BIN"
# Fully detach stdio so the child outlives this script.  Without redirecting,
# closing the script's stdin/out/err can take mars down with it on macOS.
# nohup additionally ignores SIGHUP for safety.
nohup "$BIN" > /dev/null 2>&1 < /dev/null &
disown

# Wait for the process to actually appear in the process list before returning,
# so callers chaining `./bin/run.sh && pgrep mars` don't race.  Up to ~2s.
for _ in $(seq 1 40); do
  pgrep -x mars > /dev/null && exit 0
  sleep 0.05
done
echo "warn: mars did not appear in process list within 2s" >&2
exit 1
