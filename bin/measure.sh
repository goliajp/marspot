#!/usr/bin/env bash
# Cross-terminal throughput measurement.
#
# For each (terminal, scenario) pair: spawn the terminal, type/inject a
# `time cat scenario` command, and capture the shell-side `time`
# output.  The terminal's drain rate appears as cat's elapsed time
# because cat blocks on PTY writes when the terminal can't keep up
# (this is the vtebench trick — works without instrumenting the
# terminal binaries).
#
# Output: one JSON object per (terminal, scenario) run, plus a
# Markdown comparison table that gets folded into docs/perf.md.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCENARIOS_DIR="$ROOT/bench/scenarios"
RESULTS_DIR="$ROOT/bench/results"
mkdir -p "$RESULTS_DIR"

SCENARIOS=(cat-ascii cat-mixed cat-cjk cat-emoji)
# AppleScript dispatch to iTerm/Warp/Terminal.app is fragile (AppleEvent
# timeouts, profile-specific shell init differences), so for now we
# automate mars and print paste-ready commands for the others — see
# docs/perf.md → "Cross-terminal comparison" for the manual flow.
TERMINALS=(mars)

# Number of trials per (terminal, scenario).  We keep only the median.
TRIALS=3

# ---- per-terminal launchers ---------------------------------------------

# All launchers share this contract: they get a scenario path + a marker
# path, they make the targeted terminal run
#   { time cat <scenario_path> ; } 2> <marker_path>; exit
# in a fresh window, and they return after the marker file is non-empty.

wait_for_marker() {
  local marker=$1
  for _ in $(seq 1 360); do  # up to 180s — slow terminals on 32MB scenarios take a while
    [[ -s "$marker" ]] && return 0
    sleep 0.5
  done
  return 1
}

run_in_mars() {
  local scenario=$1
  local marker=$2

  # Drive the single-session `mcli` binary, NOT the 9-session `mars`
  # app.  `mars` runs MARS_SHELL in every cell (3×3 = 9 parallel
  # cats), so the per-scenario "live throughput" timed via marker
  # would be ~1/9 of the real per-session number.  The product-level
  # multi-session test lives in `bin/scenarios/multi-session-9x.sh`.
  local cmd_script="/tmp/mars-bench-cmd.sh"
  cat > "$cmd_script" <<EOF
#!/bin/sh
/usr/bin/time -p /bin/cat "$scenario" 2> "$marker"
EOF
  chmod +x "$cmd_script"

  killall mcli mars 2>/dev/null || true
  sleep 0.3

  # Forward MARS_PROFILE through if set, so a profiling run can
  # capture per-trial counters under the harness.
  local profile_env=""
  if [[ -n "${MARS_PROFILE:-}" ]]; then
    local pf="${MARS_PROFILE}.${scenario}.${trial:-x}"
    profile_env="MARS_PROFILE=$pf"
  fi
  (cd "$ROOT" && env $profile_env MARS_SHELL="$cmd_script" \
    nohup target/release/mcli > /dev/null 2>&1 < /dev/null &)
  disown || true

  if ! wait_for_marker "$marker"; then
    killall mcli 2>/dev/null || true
    return 1
  fi
  killall mcli 2>/dev/null || true
  sleep 0.3
}

run_in_iterm() {
  local scenario=$1
  local marker=$2

  osascript <<APPLESCRIPT >/dev/null
tell application "iTerm"
  activate
  create window with default profile
  tell current session of current window
    write text "/usr/bin/time -p /bin/cat $scenario 2> $marker; sleep 0.2; exit"
  end tell
end tell
APPLESCRIPT

  if ! wait_for_marker "$marker"; then
    return 1
  fi
}

run_in() {
  local terminal=$1; shift
  case "$terminal" in
    mars)  run_in_mars  "$@" ;;
    iterm) run_in_iterm "$@" ;;
    *)     echo "unknown terminal: $terminal" >&2; return 2 ;;
  esac
}

# ---- timing parser -------------------------------------------------------

# `time` writes lines like:
#     real  0m1.234s
#     user  0m0.123s
#     sys   0m0.045s
# Convert real time to nanoseconds.

parse_real_ns() {
  local marker=$1
  # `/usr/bin/time -p` writes POSIX format (`real 1.234`), zsh/bash
  # builtin `time` writes `real 0m1.234s`.  Handle both.
  awk '
    /real/ {
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
      printf "%d", total * 1e9
      exit
    }
  ' "$marker"
}

# ---- median over trials --------------------------------------------------

median() {
  python3 -c "
import sys
xs = sorted(int(x) for x in sys.argv[1:])
print(xs[len(xs)//2])
" "$@"
}

# ---- main ----------------------------------------------------------------

# macOS default bash is 3.2 (no associative arrays), so we keep all
# per-run results in a temp dir keyed by filename, then aggregate.
RUN_DIR=$(mktemp -d)
trap 'rm -rf "$RUN_DIR"' EXIT

for scenario in "${SCENARIOS[@]}"; do
  scenario_path="$SCENARIOS_DIR/$scenario.bin"
  if [[ ! -f "$scenario_path" ]]; then
    echo "missing $scenario_path — run bin/gen-scenarios.sh first" >&2
    exit 2
  fi
  scenario_bytes=$(stat -f%z "$scenario_path")

  for terminal in "${TERMINALS[@]}"; do
    for trial in $(seq 1 $TRIALS); do
      marker="/tmp/measure-${terminal}-${scenario}-${trial}.txt"
      rm -f "$marker"
      echo "==> $terminal $scenario trial $trial/$TRIALS"

      if ! run_in "$terminal" "$scenario_path" "$marker"; then
        echo "    failed/timeout"
        continue
      fi

      ns=$(parse_real_ns "$marker")
      if [[ -z "$ns" || "$ns" == "0" ]]; then
        echo "    couldn't parse timing from $marker"
        continue
      fi
      echo "$ns" >> "$RUN_DIR/${terminal}__${scenario}.ns"
      bytes_per_sec=$((scenario_bytes * 1000000000 / ns))
      printf "    real=%.3fs  %.1f MB/s\n" \
        "$(echo "scale=3; $ns/1000000000" | bc)" \
        "$(echo "scale=1; $bytes_per_sec/1048576" | bc)"
    done
  done
done

# ---- write results -------------------------------------------------------

OUT_JSON="$RESULTS_DIR/cross-terminal.json"
python3 - "$RUN_DIR" "$SCENARIOS_DIR" "${SCENARIOS[*]}" "${TERMINALS[*]}" > "$OUT_JSON" <<'PY'
import json, os, sys
run_dir, scenarios_dir, scenarios_str, terminals_str = sys.argv[1:5]
scenarios = scenarios_str.split()
terminals = terminals_str.split()
out = {}
for scenario in scenarios:
    bytes_total = os.path.getsize(os.path.join(scenarios_dir, scenario + ".bin"))
    out[scenario] = {"bytes": bytes_total}
    for term in terminals:
        path = os.path.join(run_dir, f"{term}__{scenario}.ns")
        if not os.path.exists(path):
            out[scenario][term] = {"median_ns": 0, "bytes_per_sec": 0, "samples": []}
            continue
        samples = sorted(int(x) for x in open(path).read().split() if x.strip())
        if not samples:
            out[scenario][term] = {"median_ns": 0, "bytes_per_sec": 0, "samples": []}
            continue
        med = samples[len(samples)//2]
        bps = bytes_total * 1_000_000_000 // med if med > 0 else 0
        out[scenario][term] = {"median_ns": med, "bytes_per_sec": bps, "samples": samples}
print(json.dumps(out, indent=2))
PY

echo
echo "==> $OUT_JSON"
/bin/cat "$OUT_JSON"
echo
echo "---"
echo "Cross-terminal comparison: paste these in iTerm2 / Warp / Terminal.app"
echo "and capture each /tmp/<terminal>-<scenario>.txt as a marker file."
echo "(See docs/perf.md for how to fold the numbers in.)"
for s in "${SCENARIOS[@]}"; do
  spath="$SCENARIOS_DIR/$s.bin"
  echo
  echo "  $s ($(stat -f%z "$spath") bytes):"
  echo "    /usr/bin/time -p /bin/cat $spath 2> /tmp/<terminal>-$s.txt"
done
