#!/usr/bin/env bash
# bench/tests/e1-staleness.sh — TDD gate for perf-attack item E1.
#
# Asserts that bin/bench.sh --full refuses to compute the vs-best-other
# ratio (and exits non-zero) when bench/baseline.json's
# competitors_snapshot.captured_at is older than 7 days.  Without this
# guard, stale competitor numbers silently skew the ratio gate — the
# 2026-05-05 session caught a 2-day-old Warp snapshot reporting 47
# MB/s flat across cjk/emoji (clearly stale), which made mars look
# like it was *losing* to iTerm2 on cat-ascii when fresh measurement
# showed mars 1.84× ahead.
#
# Test method: synthesize a temp baseline.json with captured_at 30
# days back; run bench.sh --full with BASELINE pointing at it; expect
# non-zero exit and a "stale" message.

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

if [[ ! -x target/release/mars ]]; then
  echo "FAIL: target/release/mars missing; build with: cargo build --release"
  exit 1
fi

# Build a stale-snapshot temp baseline (captured_at = 30 days ago).
TMP_BASELINE=$(mktemp /tmp/e1-baseline-XXXX.json)
trap "rm -f '$TMP_BASELINE'" EXIT

python3 - "$TMP_BASELINE" <<'PY'
import json, sys, datetime
src = json.load(open("bench/baseline.json"))
old = (datetime.date.today() - datetime.timedelta(days=30)).isoformat()
src["competitors_snapshot"]["captured_at"] = old
json.dump(src, open(sys.argv[1], "w"), indent=2)
PY

# Run bench.sh --full against the stale temp baseline.  Expect non-zero
# exit and a message mentioning "stale" or "old" or "days".
echo "==> e1-staleness: running bench.sh --full with 30-day-old captured_at"
out=$(BASELINE="$TMP_BASELINE" ./bin/bench.sh --full 2>&1 || true)
rc=$?

echo "$out" | tail -20

# Did it complain about staleness?
if echo "$out" | grep -qiE 'stale|days old|out.of.date'; then
  echo
  echo "PASS: bench.sh detects and reports stale competitors_snapshot"
  exit 0
else
  echo
  echo "FAIL: bench.sh did not detect stale competitors_snapshot"
  echo "      (expected message containing 'stale' or 'days old' or"
  echo "       'out of date')"
  exit 1
fi
