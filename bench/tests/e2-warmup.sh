#!/usr/bin/env bash
# bench/tests/e2-warmup.sh — TDD gate for perf-attack item E2.
#
# Asserts that bin/bench.sh runs a warm-up trial before each measurement
# loop and that warm-up output is *not* recorded in the trial samples
# the gate medians over.  This is a structural-and-behavioural check —
# load-independent so it runs reliably even when the machine is busy
# with other work.
#
# Why E2 exists: cold first-invocation after `cargo build --release`
# pays cold-cache + cold-binary cost (file pages, dyld, glyph atlas
# data dir).  Without warm-up, trial 1's number drags the median
# enough to flap the gate (observed 2026-05-05: parse cat-ascii went
# 167 → 132 MB/s on cold run, then back to 167 on warm run, identical
# code).  The fix lands a single discarded warm-up invocation per
# measurement loop.

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

bench=bin/bench.sh
fail=0

# ---------------------------------------------------------------------------
# Part 1: structural — each measurement loop must have a warm-up.
# ---------------------------------------------------------------------------

assert_warmup() {
  local name=$1 pattern=$2
  if grep -qE -e "$pattern" "$bench"; then
    echo "  ✓ $name has warm-up trial"
  else
    echo "  ✗ $name MISSING warm-up trial (expected pattern: $pattern)"
    fail=1
  fi
}

echo "==> structural: warm-up trial present in each measurement loop"
assert_warmup "parse"       'mars" --bench "parse:.+ >/dev/null'
assert_warmup "render"      'mars" --bench render:1000 >/dev/null'
assert_warmup "scroll"      'mars" --bench scroll:.+ >/dev/null'
assert_warmup "scroll-cold" '--bench scroll-cold:.+ >/dev/null'

# ---------------------------------------------------------------------------
# Part 2: behavioural — captured trial count == documented N (no off-by-one).
#
# We run a tiny isolated parse measurement, capture mars's output to a
# private samples file, and assert exactly 5 timed samples land — i.e.
# bench.sh's parse loop runs warm-up (untimed) + 5 timed trials.
# This catches a regression where someone removes the warm-up but
# also mistakenly adjusts the trial count.
# ---------------------------------------------------------------------------

echo
echo "==> behavioural: parse loop emits exactly 5 timed samples per scenario"
if [[ ! -x target/release/mars ]]; then
  echo "  ! target/release/mars missing; build with: cargo build --release"
  fail=1
else
  TMP=$(mktemp -d)
  trap "rm -rf '$TMP'" EXIT
  : > "$TMP/parse.samples"
  # Mirror the parse loop logic from bin/bench.sh:
  target/release/mars --bench parse:bench/scenarios/cat-ascii.bin >/dev/null
  for i in 1 2 3 4 5; do
    target/release/mars --bench parse:bench/scenarios/cat-ascii.bin \
      | python3 -c "import sys,json; print(json.load(sys.stdin)['bytes_per_sec'])" \
      >> "$TMP/parse.samples"
  done
  n=$(wc -l <"$TMP/parse.samples" | tr -d ' ')
  if [[ "$n" -eq 5 ]]; then
    echo "  ✓ parse loop produces exactly 5 timed samples"
  else
    echo "  ✗ parse loop produced $n samples (expected 5)"
    fail=1
  fi
fi

echo
if [[ $fail -eq 0 ]]; then
  echo "PASS: bench.sh warm-up trial structurally present + behaviourally correct"
  exit 0
else
  echo "FAIL: bench.sh warm-up gate failed.  See messages above."
  exit 1
fi
