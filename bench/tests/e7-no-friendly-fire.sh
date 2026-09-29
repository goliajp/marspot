#!/usr/bin/env bash
# bench/tests/e7-no-friendly-fire.sh — TDD gate for perf-attack E7.
#
# Asserts that bench scripts do NOT kill mars / mcli by name (`pkill
# -x` or `killall`).  By-name killing is friendly-fire: a sanity
# bench.sh invocation while an active-9x-soak --extended is in flight
# would clobber the soak's mars binary, invalidating that run.  Use
# PID-targeted `kill "$pid"` instead so each script manages only the
# instances it spawned.
#
# Discovered 2026-05-05: a fast-gate sanity bench.sh during a 30-min
# --extended run killed mars at t=41 s (matched bench.sh's idle-RSS
# loop pkill -x), turning 1759 s of remaining samples into zero-RSS
# rows and the drift gate into a 0.0 ✓ false-pass.

set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

fail=0

echo "==> E7: no by-name kill of mars/mcli in bench scripts"

# Search bench scripts for friendly-fire patterns
files=(bin/bench.sh bin/measure.sh bin/measure-other.sh bin/bench-run.sh)
patterns=(
  'pkill -x mars'
  'pkill -x mcli'
  'pkill -x "\$bin"'      # interpolated; looped over mars/mcli
  'killall mars'
  'killall mcli'
)

for f in "${files[@]}"; do
  [[ -f "$f" ]] || continue
  for p in "${patterns[@]}"; do
    # Exclude comment lines (^\s*#) — those may document past behavior
    matches=$(grep -nE -- "$p" "$f" 2>/dev/null | grep -vE '^\s*[0-9]+:\s*#')
    if [[ -n "$matches" ]]; then
      echo "  ✗ $f has friendly-fire pattern: $p"
      echo "$matches" | sed 's/^/      /'
      fail=1
    fi
  done
done

if [[ $fail -eq 0 ]]; then
  echo "  ✓ no by-name kill of mars/mcli in any bench script"
fi

echo
if [[ $fail -eq 0 ]]; then
  echo "PASS: bench scripts are friendly-fire-safe"
  exit 0
else
  echo "FAIL: bench scripts will clobber parallel mars/mcli instances"
  exit 1
fi
