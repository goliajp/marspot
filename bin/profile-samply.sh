#!/usr/bin/env bash
# bin/profile-samply.sh — sampling profile with flamegraph UI.
#
# Complements bin/profile-live.sh: that one uses macOS `sample` and
# emits a textual report suitable for quoting into the perf notes.
# This one uses samply, which writes a Speedscope/Firefox-profiler
# JSON and opens it in a browser — navigating wide flat profiles is
# far easier graphically.
#
# Usage:
#   bin/profile-samply.sh                # default duration 10 s
#   bin/profile-samply.sh 30             # 30 s
#   bin/profile-samply.sh --attach <pid> # attach to any running process
#   PROFILE_PROC=core bin/profile-samply.sh  # the core, not the session
#
# Output: bench/results/profiles/samply-<ts>.json.gz
# Open later with: samply load <file>
#
# Without --attach it launches the product in a bench sandbox
# (bin/drivers/marspot.sh run-app) whose pane runs the same cat-ascii
# loop as profile-live.sh, and records the pane's marspot-session (or
# the core) for the duration.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"

DURATION_S=10
ATTACH_PID=""
case "${1:-}" in
  --attach) ATTACH_PID="${2:?--attach needs a pid}" ;;
  '')       ;;
  *)        DURATION_S="$1" ;;
esac

mkdir -p "$RESULTS_DIR/profiles"
OUT="$RESULTS_DIR/profiles/samply-$(date +%Y%m%d-%H%M%S).json.gz"

if ! command -v samply >/dev/null; then
  echo "samply not installed: cargo install samply" >&2
  exit 2
fi

if [[ -n "$ATTACH_PID" ]]; then
  echo "==> samply attach pid=$ATTACH_PID → $OUT (Ctrl-C to stop)"
  exec samply record --save-only -o "$OUT" -p "$ATTACH_PID"
fi

# Self-contained workload.
SCN="$SCENARIOS_DIR/cat-ascii.bin"
[[ -f "$SCN" ]] || "$ROOT/bin/gen-scenarios.sh" >/dev/null

PROC="marspot-${PROFILE_PROC:-session}"
for b in marspot-shell marspot-core marspot-session; do
  if [[ ! -x "$(marspot_bin "$b")" ]]; then
    ( cd "$ROOT" && cargo build --release --bin "$b" 2>&1 | tail -3 )
  fi
done

WORKER=$(mktemp -t marspot-samply-worker.XXXXXX.sh)
cat > "$WORKER" <<EOF
#!/bin/sh
# cat-ascii in a tight loop for a little longer than the recording
end=\$(($(date +%s) + $DURATION_S + 5))
while [ \$(date +%s) -lt \$end ]; do
  /bin/cat "$SCN" > /dev/null 2>&1 || true
done
EOF
chmod +x "$WORKER"
trap '{ kill_app marspot; rm -f "${WORKER:?}"; } || true' EXIT INT TERM

kill_app marspot || true
"$ROOT/bin/drivers/marspot.sh" run-app "$WORKER"
target_pid=""
for _ in $(seq 1 50); do
  for p in $(pids_of marspot); do
    if [[ $(ps -o comm= -p "$p" 2>/dev/null) == *"/$PROC" ]]; then target_pid=$p; break; fi
  done
  [[ -n "$target_pid" ]] && break
  sleep 0.2
done
if [[ -z "$target_pid" ]]; then
  echo "profile-samply: $PROC failed to start" >&2
  exit 1
fi

echo "==> samply record $PROC pid=$target_pid for ${DURATION_S}s → $OUT"
samply record --save-only -o "$OUT" -p "$target_pid" &
samply_pid=$!
sleep "$DURATION_S"
kill -INT "$samply_pid" 2>/dev/null || true
wait "$samply_pid" 2>/dev/null || true

echo "==> profile saved: $OUT"
echo "    view with: samply load $OUT"
