#!/usr/bin/env bash
# Cross-terminal companion to bin/measure.sh.
#
# AppleScript-driven launching of iTerm2 / Warp / Terminal.app is too
# fragile for benching (AppleEvent timeouts, profile-specific shell
# init, smart-paste rewriting commands, alias expansion of `cat`).
#
# Pragmatic alternative: print a single command line per terminal.
# Open the target terminal, paste, hit return.  The line runs all four
# `cat-*` scenarios with `/usr/bin/time -p /bin/cat` and writes to a
# known marker.  This script polls for that marker, parses the timings,
# and prints a comparison table.
#
# Usage:
#   bin/measure-other.sh
#     -> prints commands, waits for markers (Ctrl-C to abort)
#   bin/measure-other.sh --print-only
#     -> just print, don't wait

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCENARIOS_DIR="$ROOT/bench/scenarios"
RESULTS_DIR="$ROOT/bench/results"
mkdir -p "$RESULTS_DIR"

SCENARIOS=(cat-ascii cat-mixed cat-cjk cat-emoji)
# terminal.app is the OS-vendor reference floor — ships with macOS, no
# downloads, defines the bar mars needs to clear. iterm2 / warp are the
# real competitors. All three driven by the same paste-and-wait
# protocol; for fully-automated runs see bin/drivers/<term>.sh.
OTHER_TERMINALS=(terminal iterm warp)

PRINT_ONLY=0
for arg in "${@:-}"; do
  case "$arg" in
    --print-only) PRINT_ONLY=1 ;;
  esac
done

# Number of timed trials per (terminal, scenario).  Median is taken over
# these so single-shot variance can't shift the verdict (perf-attack E5).
TRIALS=${TRIALS:-3}

# E4: stale-marker cleanup.  Pre-existing /tmp/measure-{terminal}-all.txt
# files from earlier runs that still contain ==ALL_DONE== would be
# picked up immediately by the polling loop, polluting fresh data.
# Remove them up front; the build_command's `rm -f $marker` provides a
# second line of defence at paste time.
for t in "${OTHER_TERMINALS[@]}"; do
  rm -f "/tmp/measure-${t}-all.txt"
done

build_command() {
  # Single one-liner that runs all scenarios, writes timings to one
  # marker, and finishes with an ALL_DONE sentinel so the polling side
  # knows it's safe to parse.  Uses absolute paths so the user's shell
  # aliases / PATH don't matter.  Each scenario runs $TRIALS trials so
  # the parser can take the median (perf-attack E5).
  local marker=$1
  local cmd=""
  cmd+="rm -f $marker; "
  for s in "${SCENARIOS[@]}"; do
    local trial
    for ((trial = 1; trial <= TRIALS; trial++)); do
      cmd+="echo '==SCN== $s' >> $marker; "
      cmd+="/usr/bin/time -p /bin/cat $SCENARIOS_DIR/$s.bin 2>> $marker; "
    done
  done
  cmd+="echo '==ALL_DONE==' >> $marker"
  echo "$cmd"
}

print_paste_block() {
  local term=$1
  local marker="/tmp/measure-${term}-all.txt"
  echo
  echo "----------------------------------------------------------------"
  echo "  $term — open a fresh window, paste, hit return:"
  echo "----------------------------------------------------------------"
  build_command "$marker"
  echo
}

wait_for_marker() {
  local marker=$1
  local timeout_s=${2:-300}
  local start
  start=$(date +%s)
  while true; do
    if [[ -s "$marker" ]] && grep -q "==ALL_DONE==" "$marker"; then
      return 0
    fi
    local now
    now=$(date +%s)
    if (( now - start > timeout_s )); then
      return 1
    fi
    sleep 1
  done
}

parse_marker() {
  # Walk a marker and emit `<scenario>:<median_ns>` per line, with the
  # median computed across however many trials the marker contains
  # for that scenario (perf-attack E5).  Format we wrote:
  #   ==SCN== cat-ascii
  #   real X.XX        <- trial 1
  #   user ...
  #   sys ...
  #   ==SCN== cat-ascii
  #   real Y.YY        <- trial 2
  #   ...
  local marker=$1
  awk '
    /^==SCN==/ { current = $2; next }
    /^real/ {
      s = $2
      if (s ~ /m/) {
        split(s, parts, "m")
        mins = parts[1]+0
        sub("s", "", parts[2])
        secs = parts[2]+0
        total = mins*60 + secs
      } else {
        total = s+0
      }
      ns = total * 1e9
      n[current]++
      samples[current, n[current]] = ns
      # Preserve scenario order as first encountered.
      if (!(current in seen)) {
        seen[current] = 1
        order[++ocount] = current
      }
    }
    END {
      for (oi = 1; oi <= ocount; oi++) {
        k = order[oi]
        cnt = n[k]
        for (i = 1; i <= cnt; i++) v[i] = samples[k, i]
        # In-place insertion sort (cnt is small, typically 3).
        for (i = 2; i <= cnt; i++) {
          key = v[i]; j = i - 1
          while (j >= 1 && v[j] > key) { v[j+1] = v[j]; j-- }
          v[j+1] = key
        }
        mid = int((cnt + 1) / 2)
        printf "%s:%d\n", k, v[mid]
      }
    }
  ' "$marker"
}

# ---- main ----------------------------------------------------------------

for s in "${SCENARIOS[@]}"; do
  if [[ ! -f "$SCENARIOS_DIR/$s.bin" ]]; then
    echo "missing $SCENARIOS_DIR/$s.bin — run bin/gen-scenarios.sh first" >&2
    exit 2
  fi
done

for t in "${OTHER_TERMINALS[@]}"; do
  print_paste_block "$t"
done

if [[ $PRINT_ONLY -eq 1 ]]; then
  exit 0
fi

echo "----------------------------------------------------------------"
echo "Waiting for markers (Ctrl-C to abort).  Each window finishes when"
echo "you see all 4 scenario outputs scroll past."
echo "----------------------------------------------------------------"

OUT_JSON="$RESULTS_DIR/cross-terminal-other.json"
echo "{" > "$OUT_JSON"
first=1
for t in "${OTHER_TERMINALS[@]}"; do
  marker="/tmp/measure-${t}-all.txt"
  echo "==> waiting on $marker"
  if wait_for_marker "$marker" 600; then
    echo "    done"
    [[ $first -eq 0 ]] && echo "," >> "$OUT_JSON"
    first=0
    printf '  "%s": {' "$t" >> "$OUT_JSON"
    inner=1
    while IFS=":" read -r scenario ns; do
      [[ -z "$scenario" ]] && continue
      bytes=$(stat -f%z "$SCENARIOS_DIR/$scenario.bin")
      bps=$(( bytes * 1000000000 / ns ))
      mb=$(echo "scale=1; $bps/1048576" | bc)
      [[ $inner -eq 0 ]] && printf "," >> "$OUT_JSON"
      inner=0
      printf '"%s":{"median_ns":%s,"bytes_per_sec":%s}' "$scenario" "$ns" "$bps" >> "$OUT_JSON"
      printf "    %-12s %s ns  %s MB/s\n" "$scenario" "$ns" "$mb"
    done < <(parse_marker "$marker")
    printf '}' >> "$OUT_JSON"
  else
    echo "    timeout — skipping"
  fi
done
echo >> "$OUT_JSON"
echo "}" >> "$OUT_JSON"
echo
echo "==> $OUT_JSON"
/bin/cat "$OUT_JSON"
