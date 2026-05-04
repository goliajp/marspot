#!/usr/bin/env bash
# bin/scenarios/active-9x-soak.sh — 9 sessions actively producing output
# over a sustained window.  Asserts no drift (RSS / CPU stay flat over
# time) — the explicit "cannot get slower the longer it runs" check
# under active multi-session load.
#
# Why this scenario exists: idle-9x catches CPU/RSS regressions when
# sessions are idle (the easy case).  multi-session-9x catches
# burst-throughput regressions.  Neither catches "9 sessions all
# actively in use for hours" — the realistic 9-grid Claude-Code
# workflow.  This scenario fills that gap.
#
# Workload (per session, identical script — randomisation noise across
# 9 instances gives variety; deterministic enough to reproduce):
#   - 24 lines/s of mixed-colour ANSI output (~3 KiB/s base)
#   - every 10th iteration, a 60-line burst (simulates compile / log
#     dump)
#   - average aggregate output across 9 sessions: ~30 KiB/s
#   - per-session line count over default 5 min: ~10 800 lines
#     (under the 26 624-slot scrollback cap; --extended 30 min gets
#     ~65 K lines = 2.5 ring wraps)
#
# Drift assertions (q4/q1 ratio across DURATION) — sliding by mode:
#   --quick (60 s):  RSS ≤ 1.50× / CPU ≤ 3.0× — just-don't-explode
#                    sanity check; the sample window is too short for
#                    the page-commit transient to wash out.
#   default (5 min): RSS ≤ 1.30× / CPU ≤ 2.0× — mostly past the
#                    initial commit; some lazy-fault still expected.
#   --extended (30 min): RSS ≤ 1.10× / CPU ≤ 1.50× — past steady-state;
#                    a real leak would show its slope clearly.
# CPU drift is computed only when q1 mean > 1 %; below that the ratio
# is dominated by sub-percent measurement noise and isn't meaningful.
#
# Cross-terminal: mars + iterm + terminal.  warp excluded (no
# 9-windows driver; paste-mode is unreliable for indefinite-loop
# workers).
#
# Usage:
#   bin/scenarios/active-9x-soak.sh <terminal> <out-json> [--quick|--extended]
#   --quick      60 s   sample interval 5 s →  12 samples (dev iteration)
#   default     300 s   sample interval 5 s →  60 samples
#   --extended 1800 s   sample interval 5 s → 360 samples (release / nightly)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"

terminal=${1:?usage: active-9x-soak.sh <terminal> <out-json> [--quick|--extended]}
out_json=${2:?usage: active-9x-soak.sh <terminal> <out-json> [--quick|--extended]}
mode=${3:-normal}

case "$mode" in
  --extended) DURATION_S=1800 ;;
  --quick)    DURATION_S=60   ;;
  *)          DURATION_S=300; mode=normal ;;
esac
SAMPLE_INTERVAL_S=5

N=9
RUN_DIR="$MARKER_PREFIX/active-9x-soak-$terminal-$$"
rm -rf "$RUN_DIR"; mkdir -p "$RUN_DIR"
RUN_TAG="mars-bench-active9-$$"

# ---- worker -----------------------------------------------------------
#
# Indefinite-loop active worker.  Killed by either kill_app (mars) or
# close-windows (iterm/terminal) once the sample loop hits its deadline.
# `set -e` is intentionally NOT set: we want the loop to keep going even
# if one printf hits a closed pipe at shutdown.
WORKER="$RUN_DIR/worker.sh"
cat > "$WORKER" <<EOF
#!/bin/sh
printf '\033]0;%s\007' "$RUN_TAG"
ITER=0
while :; do
  ITER=\$((ITER + 1))
  # 12 mixed-colour lines per iteration.
  for j in 1 2 3 4 5 6 7 8 9 10 11 12; do
    color=\$(( (ITER + j) % 6 + 1 ))
    printf '\033[3%dm[%05d-%02d]\033[0m active session content here filler text\n' \\
      "\$color" "\$ITER" "\$j"
  done
  # Every 10th iteration, a 60-line burst (simulates compile output).
  if [ \$((ITER % 10)) -eq 0 ]; then
    i=1
    while [ \$i -le 60 ]; do
      printf '\033[2m  burst-line-%05d-%05d filler\033[0m\n' "\$ITER" "\$i"
      i=\$((i + 1))
    done
  fi
  sleep 0.5
done
EOF
chmod +x "$WORKER"

# ---- baseline ---------------------------------------------------------

baseline_kib=$(rss_total_kib "$terminal" || echo 0)
[[ -z "$baseline_kib" ]] && baseline_kib=0

# ---- dispatch ---------------------------------------------------------

WIN_IDS_FILE="$RUN_DIR/window-ids.txt"
dispatch() {
  case "$terminal" in
    mars)
      kill_app mars || true
      "$ROOT/bin/drivers/mars.sh" run-shell "$WORKER"
      ;;
    iterm)
      "$ROOT/bin/drivers/iterm.sh" run-windows "$N" "$WORKER" > "$WIN_IDS_FILE"
      ;;
    terminal)
      "$ROOT/bin/drivers/terminal.sh" run-windows "$N" "$WORKER" > "$WIN_IDS_FILE"
      ;;
    *)
      echo "active-9x-soak: unsupported terminal: $terminal" >&2
      exit 2
      ;;
  esac
}

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

trap '{ cleanup_windows; rm -rf "$RUN_DIR"; restore_focus_to "$USER_APP"; } || true' EXIT INT TERM
USER_APP=$(current_frontmost_app)
dispatch
restore_focus_to "$USER_APP"
echo "==> warming up: 30 s for $N sessions to spawn + initial commit"
sleep 30

# ---- sampling ---------------------------------------------------------

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
echo "==> sampling every ${SAMPLE_INTERVAL_S}s for ${DURATION_S}s ($mode mode)"
trap 'echo "interrupted, aggregating what we have…" >&2' INT
while [[ $(date +%s) -lt $deadline ]]; do
  t=$(( $(date +%s) - t0 ))
  rss=$(rss_total_kib "$terminal" || echo 0); [[ -z "$rss" ]] && rss=0
  cpu=$(sum_cpu_pct "$terminal")
  printf '{"t_s":%s,"rss_KiB":%s,"cpu_pct":%s}\n' "$t" "$rss" "$cpu" \
    >> "$samples_jsonl"
  sleep "$SAMPLE_INTERVAL_S"
done
trap - INT

# Tear down: mars killed directly; foreign-terminal windows closed by id.
case "$terminal" in
  mars)              kill_app mars || true ;;
  iterm|terminal)    cleanup_windows ;;
esac

# ---- aggregate -------------------------------------------------------

python3 - "$samples_jsonl" "$terminal" "$out_json" \
                          "$baseline_kib" "$DURATION_S" "$N" "$mode" "$RUN_TAG" <<'PY'
import json, sys, statistics

(samples_path, terminal, out_json,
 baseline_kib, duration_s, n, mode, run_tag) = sys.argv[1:9]
baseline_kib = int(baseline_kib); duration_s = int(duration_s); n = int(n)

samples = []
with open(samples_path) as f:
    for line in f:
        line = line.strip()
        if line:
            samples.append(json.loads(line))

if not samples:
    print("active-9x-soak: no samples — dispatch / sampling failed", file=sys.stderr)
    sys.exit(1)

rss_delta = [s["rss_KiB"] - baseline_kib for s in samples]
cpu = [s["cpu_pct"] for s in samples]

# Drift: q4/q1 mean ratio.  At least 4 samples are required for a
# meaningful split; with 12 samples (--quick) each quarter is 3 wide,
# noisy but enough to flag a clear trend.
q = max(len(samples) // 4, 1)
def q_mean(xs, last):
    s = xs[-q:] if last else xs[:q]
    return statistics.mean(s) if s else 0.0

rss_q1 = q_mean(rss_delta, last=False)
rss_q4 = q_mean(rss_delta, last=True)
rss_drift = (rss_q4 / rss_q1) if rss_q1 > 0 else None

cpu_q1 = q_mean(cpu, last=False)
cpu_q4 = q_mean(cpu, last=True)
cpu_drift = (cpu_q4 / cpu_q1) if cpu_q1 > 0 else None

# Tolerances slide by mode — see top-level comment.  Page commit
# transient dominates short windows, so --quick is just-don't-explode;
# only --extended is strict enough to catch a real leak.
RSS_THRESH = {"--quick": 1.50, "normal": 1.30, "--extended": 1.10}
CPU_THRESH = {"--quick": 3.0,  "normal": 2.0,  "--extended": 1.50}
rss_max = RSS_THRESH.get(mode, 1.30)
cpu_max = CPU_THRESH.get(mode, 2.0)
rss_drift_pass = rss_drift is None or rss_drift <= rss_max
# CPU ratio is meaningless when q1 < 1 %; pass through.
cpu_drift_pass = (
    cpu_drift is None
    or cpu_q1 < 1.0
    or cpu_drift <= cpu_max
)

result = {
    "scenario": f"active-{n}x-soak",
    "terminal": terminal,
    "metrics": {
        "n_sessions": n,
        "duration_s": duration_s,
        "mode": mode,
        "n_samples": len(samples),
        "rss_baseline_KiB": baseline_kib,
        "rss_delta_first_KiB": rss_delta[0],
        "rss_delta_last_KiB":  rss_delta[-1],
        "rss_delta_max_KiB":   max(rss_delta),
        "rss_q1_mean_KiB":     round(rss_q1, 1),
        "rss_q4_mean_KiB":     round(rss_q4, 1),
        "rss_drift_ratio_q4_over_q1": round(rss_drift, 4) if rss_drift is not None else None,
        "cpu_pct_mean":  round(statistics.mean(cpu), 2) if cpu else None,
        "cpu_pct_max":   round(max(cpu), 2)             if cpu else None,
        "cpu_q1_mean":   round(cpu_q1, 2),
        "cpu_q4_mean":   round(cpu_q4, 2),
        "cpu_drift_ratio_q4_over_q1": round(cpu_drift, 4) if cpu_drift is not None else None,
        "samples": samples,
        "run_tag": run_tag,
        "checks": {
            "rss_drift_under_threshold": rss_drift_pass,
            "cpu_drift_under_threshold": cpu_drift_pass,
        },
        "thresholds": {
            "rss_drift_max": rss_max,
            "cpu_drift_max": cpu_max,
        },
    },
    "skipped": [],
}

with open(out_json, "w") as f:
    json.dump(result, f, indent=2)

m = result["metrics"]; c = m["checks"]
print(f"  {terminal:8s} active-{n}x-soak ({duration_s}s {mode}, {len(samples)} samples):")
print(f"    RSS Δ first / last / max  {m['rss_delta_first_KiB']/1024:.0f} / {m['rss_delta_last_KiB']/1024:.0f} / {m['rss_delta_max_KiB']/1024:.0f} MiB  (baseline {baseline_kib/1024:.0f} MiB)")
t = result["metrics"]["thresholds"]
rss_verdict = "✓" if c["rss_drift_under_threshold"] else f"✗ FAIL >{t['rss_drift_max']}×"
cpu_verdict = "✓" if c["cpu_drift_under_threshold"] else f"✗ FAIL >{t['cpu_drift_max']}×"
print(f"    RSS drift q4/q1            {m['rss_drift_ratio_q4_over_q1']}   {rss_verdict} (threshold {t['rss_drift_max']}×)")
print(f"    CPU mean / max             {m['cpu_pct_mean']} / {m['cpu_pct_max']} %")
print(f"    CPU drift q4/q1            {m['cpu_drift_ratio_q4_over_q1']}   {cpu_verdict} (threshold {t['cpu_drift_max']}×)")
PY

exit 0
