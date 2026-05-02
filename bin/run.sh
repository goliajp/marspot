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

BIN="target/$PROFILE/mars"
echo "==> launching $BIN"
"$BIN" &
disown
