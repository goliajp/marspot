#!/usr/bin/env bash
# bin/scenarios/htop-60s.sh — htop running for N seconds (sustained
# periodic full repaint).
#
# Why this scenario exists: htop redraws its full screen ~once per
# second.  This is the canonical "long-running periodic full repaint"
# stress on terminal renderer + parser — different from the burst
# throughput cat-* tests.
#
# Method: worker runs `htop &`, sleeps the configured duration, then
# `kill <htop-pid>` (signal — no keystroke).  Worker exits cleanly,
# the bench window gets closed by id.  We sample terminal RSS / CPU
# every second during the run.
#
# Usage:
#   bin/scenarios/htop-60s.sh <terminal> <out-json> [--quick]
#
#   --quick   10 s instead of 60 s (for dev / CI smoke; not the real
#             measurement).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"

terminal=${1:?usage: htop-60s.sh <terminal> <out-json> [--quick]}
out_json=${2:?usage: htop-60s.sh <terminal> <out-json> [--quick]}
mode=${3:-normal}

case "$mode" in
  --quick) DURATION_S=10 ;;
  *)       DURATION_S=60 ;;
esac

if ! command -v htop >/dev/null; then
  echo "htop not installed — \`brew install htop\` and retry" >&2
  cat > "$out_json" <<JSON
{"scenario":"htop-60s","terminal":"$terminal","metrics":{},"skipped":["htop not installed"]}
JSON
  exit 0
fi

RUN_DIR="$MARKER_PREFIX/htop-$terminal-$$"
rm -rf "$RUN_DIR"; mkdir -p "$RUN_DIR"
WIN_IDS_FILE="$RUN_DIR/window-ids.txt"
RUN_TAG="marspot-bench-htop-$$"

WORKER="$RUN_DIR/worker.sh"
cat > "$WORKER" <<EOF
#!/bin/sh
printf '\033]0;%s\007' "$RUN_TAG"
htop &
HTOP_PID=\$!
sleep $((DURATION_S + 2))
kill \$HTOP_PID 2>/dev/null
wait \$HTOP_PID 2>/dev/null
exit 0
EOF
chmod +x "$WORKER"

cleanup_windows() {
  local ids=()
  [[ -f "$WIN_IDS_FILE" ]] || return 0
  while IFS= read -r line; do
    line=${line//[$'\r\n\t ']/}
    [[ -n "$line" ]] && ids+=("$line")
  done < "$WIN_IDS_FILE"
  (( ${#ids[@]} > 0 )) || return 0
  case "$terminal" in
    iterm)    "$ROOT/bin/drivers/iterm.sh"    close-windows "${ids[@]}" 2>/dev/null || true ;;
    terminal) "$ROOT/bin/drivers/terminal.sh" close-windows "${ids[@]}" 2>/dev/null || true ;;
  esac
}

case "$terminal" in
  marspot)     sample_proc=mcli ;;
  *)        sample_proc=$terminal ;;
esac

USER_APP=$(current_frontmost_app)
trap '{ cleanup_windows; rm -rf "$RUN_DIR"; restore_focus_to "$USER_APP"; } || true' EXIT INT TERM

baseline_kib=$(rss_total_kib "$sample_proc" || echo 0)
[[ -z "$baseline_kib" ]] && baseline_kib=0

case "$terminal" in
  marspot)
    kill_app marspot || true; kill_app mcli || true
    "$ROOT/bin/drivers/marspot.sh" run-shell-mcli "$WORKER"
    ;;
  iterm)
    "$ROOT/bin/drivers/iterm.sh" run-windows 1 "$WORKER" > "$WIN_IDS_FILE"
    ;;
  terminal)
    "$ROOT/bin/drivers/terminal.sh" run-single "$WORKER" > "$WIN_IDS_FILE"
    ;;
  *)
    echo "htop-60s: unsupported terminal: $terminal" >&2
    exit 2
    ;;
esac
restore_focus_to "$USER_APP"

# Wait briefly for htop to start drawing before sampling.
sleep 2

# ---- sample CPU + RSS for DURATION_S seconds (1 Hz) -----------------
samples_jsonl="$RUN_DIR/samples.jsonl"
: > "$samples_jsonl"

sum_cpu_pct() {
  local pids; pids=$(pids_of "$1")
  [[ -z "$pids" ]] && { echo 0; return; }
  local sum=0
  for p in $pids; do
    local c; c=$(ps -o %cpu= -p "$p" 2>/dev/null | tr -d ' ')
    [[ -n "$c" ]] && sum=$(python3 -c "print($sum + $c)")
  done
  echo "$sum"
}

t0=$(date +%s)
deadline=$(( t0 + DURATION_S ))
while [[ $(date +%s) -lt $deadline ]]; do
  t=$(( $(date +%s) - t0 ))
  rss=$(rss_total_kib "$sample_proc" || echo 0); [[ -z "$rss" ]] && rss=0
  cpu=$(sum_cpu_pct "$sample_proc")
  printf '{"t_s":%s,"rss_KiB":%s,"cpu_pct":%s}\n' "$t" "$rss" "$cpu" >> "$samples_jsonl"
  sleep 1
done

# ---- aggregate -----------------------------------------------------

python3 - "$samples_jsonl" "$terminal" "$out_json" "$baseline_kib" \
                          "$DURATION_S" "$RUN_TAG" <<'PY'
import json, os, sys, statistics

(samples_path, terminal, out_json,
 baseline_kib, duration_s, run_tag) = sys.argv[1:7]
baseline_kib, duration_s = int(baseline_kib), int(duration_s)

samples = []
if os.path.exists(samples_path):
    for line in open(samples_path):
        line = line.strip()
        if line:
            samples.append(json.loads(line))

cpu = [s["cpu_pct"] for s in samples]
rss_delta = [s["rss_KiB"] - baseline_kib for s in samples]

result = {
    "scenario": "htop-60s",
    "terminal": terminal,
    "metrics": {
        "duration_s": duration_s,
        "n_samples": len(samples),
        "cpu_pct_mean": round(statistics.mean(cpu), 2) if cpu else None,
        "cpu_pct_max":  round(max(cpu), 2) if cpu else None,
        "rss_baseline_KiB": baseline_kib,
        "rss_delta_max_KiB":  max(rss_delta) if rss_delta else None,
        "rss_delta_mean_KiB": int(statistics.mean(rss_delta)) if rss_delta else None,
        "samples": samples,
        "run_tag": run_tag,
    },
    "skipped": [] if samples else ["no samples — terminal didn't expose RSS during the run"],
}
with open(out_json, "w") as f:
    json.dump(result, f, indent=2)

m = result["metrics"]
print(f"  {terminal:8s} htop-60s ({duration_s}s, {m['n_samples']} samples):")
if m["cpu_pct_mean"] is not None:
    print(f"    CPU mean / max     {m['cpu_pct_mean']} / {m['cpu_pct_max']} %")
if m["rss_delta_max_KiB"] is not None:
    print(f"    RSS Δ mean / max   {m['rss_delta_mean_KiB']/1024:.0f} / {m['rss_delta_max_KiB']/1024:.0f} MiB  (baseline {baseline_kib/1024:.0f} MiB)")
PY

exit 0
