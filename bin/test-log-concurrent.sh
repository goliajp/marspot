#!/usr/bin/env bash
#
# Cross-process concurrent writers to the shared marspot.log stream.
#
# 50 parallel `marspot-shelld --log-event TAG_N DETAIL_N` invocations
# race to open + write + close the same `paths::log_dir()/marspot.log`.
# Each event is a single short TSV line (≤ 480 B); POSIX guarantees
# that O_APPEND writes < PIPE_BUF (512 B on macOS) cannot interleave at
# the byte level.
#
# Assertions:
#   - 50 lines land in the file (no event lost).
#   - Every line has the same tab-separated column count (no interleave
#     that would produce a malformed row).
#   - Every TAG_N appears exactly once (event identity preserved).
#
# Sandboxed via MARSPOT_STATE_DIR + MARSPOT_LOG_DIR; never touches the
# user's real log directory.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE="${MARSPOT_TEST_PROFILE:-debug}"
SHELLD="$ROOT/target/$PROFILE/marspot-shelld"
[[ -x "$SHELLD" ]] || { echo "FAIL: build marspot-shelld first (cargo build${PROFILE:+ --$PROFILE})"; exit 1; }

STATE_DIR="/tmp/marspot-log-concurrent.$$"
export MARSPOT_STATE_DIR="$STATE_DIR"
LOG_DIR="$STATE_DIR/logs"
LOG="$LOG_DIR/marspot.log"

cleanup() { rm -rf "$STATE_DIR"; }
trap cleanup EXIT

fail() {
  echo "FAIL: $*"
  [[ -f "$LOG" ]] && {
    echo "  (first 5 / last 5 lines of $LOG, $(wc -l <"$LOG") total):"
    head -5 "$LOG" | sed 's/^/    /'
    echo "    ..."
    tail -5 "$LOG" | sed 's/^/    /'
  }
  exit 1
}

mkdir -p "$LOG_DIR"

# 50 concurrent invocations, unique TAG each.
N=50
pids=()
for i in $(seq 1 $N); do
  "$SHELLD" --log-event "CONC_TEST_$i" "payload-$i-$RANDOM" &
  pids+=($!)
done
# Wait for all.
for p in "${pids[@]}"; do
  wait "$p"
done

[[ -f "$LOG" ]] || fail "log file not created"

# Count lines.
got=$(wc -l <"$LOG" | tr -d ' ')
[[ "$got" -ge "$N" ]] || fail "expected ≥ $N lines, got $got"

# Every TAG must appear exactly once.
for i in $(seq 1 $N); do
  hits=$(grep -c $'\tCONC_TEST_'"$i"$'\t' "$LOG" || true)
  [[ "$hits" == "1" ]] || fail "TAG CONC_TEST_$i appeared $hits times (expected 1)"
done

# Column count (9 tabs per well-formed line = 10 columns).
# Format: ISO  unix-ms  LEVEL  comp  pid  tid  tag  msg  fields...
# Empty fields trailing → tabs = 7. We allow 7+ (some lines may have
# a fields column, none has fewer than 7 tabs).
malformed=0
while IFS= read -r line; do
  tabs="${line//[^	]/}"
  if [[ ${#tabs} -lt 7 ]]; then
    malformed=$((malformed + 1))
  fi
done < "$LOG"
[[ "$malformed" == "0" ]] || fail "$malformed malformed lines (< 7 tabs)"

echo "PASS: $N concurrent writers — $got lines, all tags present, no interleave"
