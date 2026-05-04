#!/usr/bin/env bash
# bin/bench-run.sh — orchestrator for the persistent bench mechanism.
#
# Walks the matrix declared in docs/bench.md (scenario × terminal),
# dispatches each cell via bin/scenarios/<id>.sh, merges the results
# into one snapshot file per run, and appends a row to the
# time-series log.
#
# Usage:
#   bin/bench-run.sh
#     Run every scenario × every terminal that supports it.  Default.
#
#   bin/bench-run.sh --scenarios <ids,...> --terminals <names,...>
#     Restrict the matrix.  Useful while iterating on one scenario.
#
#   bin/bench-run.sh --quick
#     Faster matrix (idle-9x and htop-60s run --quick, soak-style
#     scenarios skipped).  For dev iteration, not pre-merge gate.
#
#   bin/bench-run.sh --extended
#     Pre-release / nightly soak.  idle-9x runs 30 min instead of
#     5 min (catches CPU / RSS drift that only shows past the
#     5 min sample window).  Run before declaring a release or as
#     a scheduled nightly to keep CLAUDE.md's "cannot get slower
#     the longer it runs" honest at the timescale that matters.
#
# Outputs:
#   bench/results/<runid>.json       full snapshot of this run
#   bench/results/timeseries.jsonl   one line per (run, scenario, terminal)
#   bench/results/cross-terminal.json  symlink to latest snapshot
#
# Doesn't touch bench/baseline.json — gate integration is bin/bench.sh's job.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"

# ---- defaults ----------------------------------------------------------

# typing-latency is intentionally NOT in the default scenarios.  It
# unavoidably steals focus to drive keystrokes (System Events is a
# global event source), and a default `/benchmark` shouldn't surprise
# the user mid-work.  Opt in with `--scenarios typing-latency` or
# `--include-typing-latency`.
DEFAULT_SCENARIOS=(multi-session-9x scrollback-1m idle-9x vim-jump htop-60s active-9x-soak)
ALL_SCENARIOS=(multi-session-9x scrollback-1m idle-9x vim-jump htop-60s active-9x-soak typing-latency)
ALL_TERMINALS=(mars iterm terminal warp)

SCENARIOS=("${DEFAULT_SCENARIOS[@]}")
TERMINALS=("${ALL_TERMINALS[@]}")
QUICK=0
EXTENDED=0
INCLUDE_TYPING=0

while (( $# > 0 )); do
  case "$1" in
    --scenarios)
      shift
      IFS=',' read -r -a SCENARIOS <<< "$1"
      shift ;;
    --terminals)
      shift
      IFS=',' read -r -a TERMINALS <<< "$1"
      shift ;;
    --quick)
      QUICK=1
      shift ;;
    --extended)
      EXTENDED=1
      shift ;;
    --include-typing-latency)
      INCLUDE_TYPING=1
      shift ;;
    -h|--help)
      sed -n '2,32p' "$0"; exit 0 ;;
    *)
      echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

if (( QUICK && EXTENDED )); then
  echo "--quick and --extended are mutually exclusive" >&2
  exit 2
fi

# ---- run id + git fingerprint -----------------------------------------

run_id="$(date +%Y%m%d-%H%M%S)-$(git rev-parse --short=7 HEAD 2>/dev/null || echo 0000000)"
git_sha=$(git rev-parse HEAD 2>/dev/null || echo unknown)
git_dirty=false
if ! git diff --quiet HEAD 2>/dev/null; then git_dirty=true; fi

machine_model=$(sysctl -n hw.model 2>/dev/null || echo unknown)
machine_cpu=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unknown)
machine_macos=$(sw_vers -productVersion 2>/dev/null || echo unknown)
machine_ram_gb=$(python3 -c "import os; print(round(os.sysconf('SC_PHYS_PAGES')*os.sysconf('SC_PAGE_SIZE')/1024/1024/1024))" 2>/dev/null || echo 0)

mkdir -p "$RESULTS_DIR"
RUN_OUT_DIR="$RESULTS_DIR/$run_id"
mkdir -p "$RUN_OUT_DIR"

echo "==> bench-run.sh $run_id"
echo "    scenarios: ${SCENARIOS[*]}"
echo "    terminals: ${TERMINALS[*]}"
echo "    quick:     $QUICK"
echo "    extended:  $EXTENDED"
echo "    git:       $git_sha (dirty=$git_dirty)"
echo "    machine:   $machine_model / $machine_cpu / macOS $machine_macos"
echo

# ---- helpers -----------------------------------------------------------

# If the user explicitly asked for typing-latency (via --include or
# --scenarios), put it back in the matrix.
if (( INCLUDE_TYPING )); then
  SCENARIOS+=(typing-latency)
fi

# Whether `scenario` supports `terminal`.  Warp is paste-mode for almost
# everything; typing-latency is mars-only by construction.
supports() {
  local scenario=$1 terminal=$2
  case "$scenario:$terminal" in
    typing-latency:mars)             return 0 ;;
    typing-latency:*)                return 1 ;;
    multi-session-9x:warp)           return 1 ;;  # paste-mode unreliable for 9-up
    scrollback-1m:warp)              return 1 ;;  # paste-mode + we don't auto-close
    idle-9x:warp)                    return 1 ;;
    vim-jump:warp)                   return 1 ;;  # no warp driver; paste-mode unreliable
    htop-60s:warp)                   return 1 ;;  # no warp driver; paste-mode unreliable
    active-9x-soak:warp)             return 1 ;;  # no warp 9-windows driver
    *)                               return 0 ;;
  esac
}

# Some scenarios run a long time — short-circuit them in --quick mode.
scenario_args() {
  local scenario=$1
  case "$scenario" in
    idle-9x|active-9x-soak)
      if   (( EXTENDED )); then echo "--extended"
      elif (( QUICK ));    then echo "--quick"
      else echo ""
      fi ;;
    htop-60s)        (( QUICK )) && echo "--quick" || echo "" ;;
    typing-latency)  (( QUICK )) && echo 50 || echo 200 ;;
    *)               echo "" ;;
  esac
}

# ---- run the matrix ---------------------------------------------------

declare -a per_cell_files=()
for scenario in "${SCENARIOS[@]}"; do
  for terminal in "${TERMINALS[@]}"; do
    if ! supports "$scenario" "$terminal"; then
      echo "==  skip: $scenario × $terminal (not supported)"
      continue
    fi
    out="$RUN_OUT_DIR/$scenario-$terminal.json"
    extra_args=$(scenario_args "$scenario")
    echo "==> $scenario × $terminal"
    if "$ROOT/bin/scenarios/$scenario.sh" "$terminal" "$out" $extra_args; then
      per_cell_files+=("$out")
    else
      echo "    cell failed (continuing)"
    fi
    echo
  done
done

# ---- assemble snapshot ------------------------------------------------

snapshot="$RESULTS_DIR/$run_id.json"
python3 - "$run_id" "$git_sha" "$git_dirty" \
        "$machine_model" "$machine_cpu" "$machine_macos" "$machine_ram_gb" \
        "$snapshot" "$RESULTS_DIR/timeseries.jsonl" \
        "${per_cell_files[@]}" <<'PY'
import json, os, sys, datetime, glob

(run_id, git_sha, git_dirty,
 machine_model, machine_cpu, machine_macos, machine_ram_gb,
 snapshot_path, timeseries_path, *cell_files) = sys.argv[1:]

snapshot = {
    "run_id": run_id,
    "started_at": datetime.datetime.now().isoformat(timespec="seconds"),
    "git_sha": git_sha,
    "git_dirty": git_dirty == "true",
    "machine": {
        "model": machine_model,
        "cpu":   machine_cpu,
        "macos": machine_macos,
        "ram_gb": int(machine_ram_gb) if machine_ram_gb.isdigit() else 0,
    },
    "scenarios": {},
}

# Each cell file is one (scenario, terminal) result.
for path in cell_files:
    if not os.path.exists(path):
        continue
    try:
        cell = json.load(open(path))
    except Exception:
        continue
    sid = cell.get("scenario", "?")
    tid = cell.get("terminal", "?")
    snapshot["scenarios"].setdefault(sid, {})[tid] = {
        "metrics": cell.get("metrics", {}),
        "skipped": cell.get("skipped", []),
    }

with open(snapshot_path, "w") as f:
    json.dump(snapshot, f, indent=2)

# Append one timeseries row per cell.  Keep rows narrow — a few
# headline metrics so a 30-commit history fits in a screenful.
HEADLINE_METRICS = {
    "multi-session-9x": ["aggregate_throughput_MBps", "wall_s", "rss_peak_delta_KiB"],
    "cat-mixed.9x":     ["aggregate_throughput_MBps", "wall_s", "rss_peak_delta_KiB"],
    "scrollback-1m":    ["push_throughput_MBps", "rss_post_delta_KiB", "disk_delta_KiB"],
    "idle-9x":          ["cpu_pct_mean", "rss_drift_ratio_q4_over_q1", "rss_delta_last_KiB"],
    "vim-jump":         ["wall_s", "rss_post_delta_KiB"],
    "htop-60s":         ["cpu_pct_mean", "cpu_pct_max", "rss_delta_max_KiB"],
    "active-9x-soak":   ["rss_drift_ratio_q4_over_q1", "cpu_drift_ratio_q4_over_q1", "cpu_pct_mean"],
    "typing-latency":   ["input_latency_us_p50", "input_latency_us_p95", "input_latency_us_p99"],
}

with open(timeseries_path, "a") as f:
    for sid, terms in snapshot["scenarios"].items():
        keys = HEADLINE_METRICS.get(sid, [])
        for tid, payload in terms.items():
            row = {
                "run_id": run_id,
                "git_sha": git_sha[:7],
                "scenario": sid,
                "terminal": tid,
            }
            for k in keys:
                row[k] = payload["metrics"].get(k)
            f.write(json.dumps(row) + "\n")

print(f"==> snapshot:    {snapshot_path}")
print(f"==> timeseries:  {timeseries_path} (+{sum(len(t) for t in snapshot['scenarios'].values())} row(s))")

# Maintain `cross-terminal.json` symlink for back-compat with bench.sh's gate.
link_path = os.path.join(os.path.dirname(snapshot_path), "cross-terminal.json")
try:
    if os.path.islink(link_path) or os.path.exists(link_path):
        os.unlink(link_path)
    os.symlink(os.path.basename(snapshot_path), link_path)
except OSError:
    pass
PY

echo
echo "==> done."
