#!/usr/bin/env bash
# bench/tests/e4-e5-measure-other.sh — TDD gates for perf-attack E4 + E5.
#
# E4: stale-marker cleanup at startup
# E5: 3-trial median per (terminal, scenario), exposed in JSON output

set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

fail=0

# ---------------------------------------------------------------------------
# E4 — stale marker is wiped at startup
# ---------------------------------------------------------------------------

echo "==> E4: stale marker cleanup"
stale=/tmp/measure-iterm-all.txt
echo "stale ==ALL_DONE==" > "$stale"
./bin/measure-other.sh --print-only > /dev/null 2>&1
if [[ -f "$stale" ]]; then
  echo "  ✗ stale marker $stale NOT removed at startup"
  fail=1
  rm -f "$stale"
else
  echo "  ✓ stale marker removed at startup"
fi

# Reset for E5
rm -f /tmp/measure-{terminal,iterm,warp}-all.txt 2>/dev/null

# ---------------------------------------------------------------------------
# E5 part 1 — print-only command repeats each scenario 3 times per terminal
# ---------------------------------------------------------------------------

echo
echo "==> E5: build_command emits 3 trials per (terminal, scenario)"
out=$(./bin/measure-other.sh --print-only 2>/dev/null)

declare -i all_ok=1
for s in cat-ascii cat-mixed cat-cjk cat-emoji; do
  n=$(echo "$out" | grep -oE "/bin/cat [^[:space:]]*${s}\\.bin" | wc -l | tr -d ' ')
  if [[ "$n" -eq 9 ]]; then
    echo "  ✓ $s referenced $n times (3 terminals × 3 trials)"
  else
    echo "  ✗ $s referenced $n times (expected 9 = 3 × 3)"
    all_ok=0
  fi
done
[[ $all_ok -eq 1 ]] || fail=1

# ---------------------------------------------------------------------------
# E5 part 2 — end-to-end: pre-populate markers with 3-trial data, run
# measure-other.sh non-interactively, verify median emerges in JSON.
# ---------------------------------------------------------------------------

echo
echo "==> E5: end-to-end median computation in JSON output"

# Cleanup any prior state
rm -f /tmp/measure-{terminal,iterm,warp}-all.txt 2>/dev/null

# Start measure-other.sh in background
mo_log=$(mktemp /tmp/e5-mo-XXXX.log)
./bin/measure-other.sh > "$mo_log" 2>&1 &
mo_pid=$!

# Give it ~1s to print paste blocks + clean stale markers + enter polling
sleep 1.5

# Now plant 3-trial fake markers for cat-ascii on each terminal.
# Trial order 0.20 / 0.50 / 1.00 — median = 0.50s = 500_000_000 ns,
# LAST = 1.00s = 1_000_000_000 ns.  These differ so a buggy parser
# that keeps "last" instead of computing median fails the assertion.
for t in terminal iterm warp; do
  cat > /tmp/measure-${t}-all.txt <<EOF
==SCN== cat-ascii
real 0.20
user 0.00
sys 0.02
==SCN== cat-ascii
real 0.50
user 0.00
sys 0.05
==SCN== cat-ascii
real 1.00
user 0.00
sys 0.10
==ALL_DONE==
EOF
done

# Wait for measure-other.sh to finish (max 30s safety)
for _ in $(seq 1 60); do
  if ! kill -0 $mo_pid 2>/dev/null; then break; fi
  sleep 0.5
done
kill $mo_pid 2>/dev/null
wait $mo_pid 2>/dev/null || true

# Verify JSON has median_ns = 750000000 for cat-ascii on each terminal
out_json=bench/results/cross-terminal-other.json
if [[ ! -f "$out_json" ]]; then
  echo "  ✗ JSON output $out_json missing"
  fail=1
else
  declare -i med_ok=1
  for t in terminal iterm warp; do
    # Pull median_ns for $t.cat-ascii using grep+sed
    val=$(python3 -c "
import json
j = json.load(open('$out_json'))
v = j.get('$t', {}).get('cat-ascii', {}).get('median_ns')
print(v if v is not None else 'NULL')
")
    if [[ "$val" == "500000000" ]]; then
      echo "  ✓ $t cat-ascii median_ns = 500_000_000"
    else
      echo "  ✗ $t cat-ascii median_ns = $val (expected 500_000_000; "
      echo "      a 'last sample wins' implementation would yield 1_000_000_000)"
      med_ok=0
    fi
  done
  [[ $med_ok -eq 1 ]] || fail=1
fi

# Cleanup
rm -f /tmp/measure-{terminal,iterm,warp}-all.txt "$mo_log" 2>/dev/null

echo
if [[ $fail -eq 0 ]]; then
  echo "PASS: E4 + E5 measure-other.sh hardening verified"
  exit 0
else
  echo "FAIL: E4/E5 gate failed; see above"
  exit 1
fi
