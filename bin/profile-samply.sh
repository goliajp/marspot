#!/usr/bin/env bash
# bin/profile-samply.sh — sampling profile with flamegraph UI.
#
# Complements bin/profile-live.sh: that one uses macOS `sample` and
# emits a textual report suitable for quoting into docs/perf.md.
# This one uses samply, which writes a Speedscope/Firefox-profiler
# JSON and opens it in a browser — navigating wide flat profiles is
# far easier graphically.
#
# Usage:
#   bin/profile-samply.sh                # default duration 10 s
#   bin/profile-samply.sh 30             # 30 s
#   bin/profile-samply.sh --attach <pid> # attach to a running mars/mcli
#
# Output: bench/results/profiles/samply-<ts>.json.gz
# Open later with: samply load <file>
#
# Launching the workload here uses the same cat-ascii loop as
# profile-live.sh — workload changes belong in one place; if you need
# a different workload, set MARS_SHELL before invoking.

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

WORKER=$(mktemp -t mars-samply-worker.XXXXXX.sh)
cat > "$WORKER" <<EOF
#!/bin/sh
# Drive mcli with cat-ascii in a tight loop for $DURATION_S seconds.
end=\$(($(date +%s) + $DURATION_S))
while [ \$(date +%s) -lt \$end ]; do
  /bin/cat "$SCN" > /dev/null 2>&1 || true
done
EOF
chmod +x "$WORKER"
trap 'rm -f "$WORKER"' EXIT INT TERM

if [[ ! -x "$(mars_bin mcli)" ]]; then
  ( cd "$ROOT" && cargo build --release --bin mcli 2>&1 | tail -3 )
fi

echo "==> samply record mcli for ~${DURATION_S}s → $OUT"
MARS_SHELL="$WORKER" samply record --save-only -o "$OUT" -- "$(mars_bin mcli)"

echo "==> profile saved: $OUT"
echo "    view with: samply load $OUT"
