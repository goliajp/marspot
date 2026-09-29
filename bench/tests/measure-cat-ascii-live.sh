#!/usr/bin/env bash
# bench/tests/measure-cat-ascii-live.sh — measure cat-ascii live MB/s.
#
# Builds mcli (release), runs cat-ascii through it via MARS_SHELL 3x,
# prints the median MB/s on stdout.  Designed for git-bisect-run usage:
# place this script outside the working tree (or use a fixed copy) so
# every checked-out commit can be measured uniformly.
#
# Exit codes:
#   0 — measurement succeeded; median MB/s on stdout
# 125 — build failure or measurement failure (skip in bisect)
#   1 — wrapper logic error

set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

if ! cargo build --release --bin mcli >/dev/null 2>&1; then
  echo "BUILD_FAIL" >&2
  exit 125
fi

scenario=$(realpath bench/scenarios/cat-ascii.bin)
if [[ ! -f "$scenario" ]]; then
  if [[ -x bin/gen-scenarios.sh ]]; then
    bin/gen-scenarios.sh >/dev/null 2>&1 || { echo "GEN_FAIL" >&2; exit 125; }
  else
    echo "SCENARIO_MISSING" >&2; exit 125
  fi
fi

bytes=$(stat -f%z "$scenario")
results=()

for trial in 1 2 3; do
  marker=$(mktemp /tmp/cat-ascii-marker-XXXX)
  cmd=/tmp/mars-bench-cmd-bisect.sh
  cat > "$cmd" <<EOF
#!/bin/sh
/usr/bin/time -p /bin/cat "$scenario" 2> "$marker"
EOF
  chmod +x "$cmd"

  killall mcli mars 2>/dev/null || true
  sleep 0.3

  MARS_SHELL="$cmd" nohup target/release/mcli > /dev/null 2>&1 < /dev/null &
  disown 2>/dev/null || true

  # Poll for ==real== line (60s safety timeout)
  ok=0
  for _ in $(seq 1 120); do
    if [[ -s "$marker" ]] && grep -q '^real' "$marker"; then
      ok=1; break
    fi
    sleep 0.5
  done
  killall mcli 2>/dev/null || true
  sleep 0.3

  if [[ $ok -eq 0 ]]; then
    echo "MEASURE_TIMEOUT trial=$trial" >&2
    rm -f "$marker"
    continue
  fi

  real=$(awk '/^real/ {print $2}' "$marker")
  rm -f "$marker"
  if [[ -z "$real" ]]; then
    echo "PARSE_FAIL trial=$trial" >&2
    continue
  fi

  mb=$(python3 -c "print($bytes / $real / 1048576)")
  results+=("$mb")
done

if [[ ${#results[@]} -eq 0 ]]; then
  echo "ALL_TRIALS_FAILED" >&2
  exit 125
fi

# Median
printf '%s\n' "${results[@]}" | python3 -c "
import sys
xs = sorted(float(x) for x in sys.stdin.read().split() if x.strip())
print(f'{xs[len(xs)//2]:.2f}')
"
