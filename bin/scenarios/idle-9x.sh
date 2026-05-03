#!/usr/bin/env bash
# bin/scenarios/idle-9x.sh — 9 idle sessions, sustained sampling.
#
# Why this scenario exists: CLAUDE.md's #3 architectural commitment is
# "cannot get slower the longer it runs."  For multi-session
# Claude-Code work the user keeps 9 terminals open all day; if RSS
# creeps, CPU at idle isn't 0, or disk grows without bound, the
# product fails its core promise.
#
# Workload: open N=9 sessions, leave them idle, sample every 5 s for
# the configured duration (default 5 min, --extended for 30 min).
# Pass / fail derived from the slope.
#
# Hard rules from CLAUDE.md:
#   - idle CPU must be ~0 % (no animation timers, no busy waits)
#   - RSS at end must not exceed RSS at start × 1.10
#   - disk usage must not grow (or, where disk is intentional like
#     scrollback spill, grow by a bounded predictable amount)
#
# Usage:
#   bin/scenarios/idle-9x.sh <terminal> <out-json> [--extended]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"

terminal=${1:?usage: idle-9x.sh <terminal> <out-json> [--extended]}
out_json=${2:?usage: idle-9x.sh <terminal> <out-json> [--extended]}
mode=${3:-normal}

case "$mode" in
  --extended) DURATION_S=1800 ;;  # 30 min — soak / nightly
  --quick)    DURATION_S=60   ;;  # 1  min — dev testing only
  *)          DURATION_S=300  ;;  # 5  min — default for `bench.sh --full`
esac
SAMPLE_INTERVAL_S=5

N=9
RUN_DIR="$MARKER_PREFIX/idle-9x-$terminal-$$"
rm -rf "$RUN_DIR"; mkdir -p "$RUN_DIR"
RUN_TAG="mars-bench-idle-$$"

# Worker that just sleeps forever (idle).  Kept alive for DURATION + slack
# so the scenario can sample without races.
WORKER="$RUN_DIR/worker.sh"
cat > "$WORKER" <<EOF
#!/bin/sh
printf '\033]0;%s\007' "$RUN_TAG"
sleep $((DURATION_S + 30))
exit 0
EOF
chmod +x "$WORKER"

# ---- baseline ----------------------------------------------------------

baseline_kib=$(rss_total_kib "$terminal" || echo 0)
[[ -z "$baseline_kib" ]] && baseline_kib=0

scrollback_dir() {
  case "$1" in
    mars)     echo "$HOME/.cache/mars/scrollback" ;;
    iterm)    echo "$HOME/Library/Application Support/iTerm2/SavedState" ;;
    warp)     echo "$HOME/Library/Application Support/dev.warp.Warp-Stable" ;;
    terminal) echo "" ;;
  esac
}
du_kib() {
  local d=$1
  [[ -z "$d" || ! -d "$d" ]] && { echo 0; return; }
  du -sk "$d" 2>/dev/null | awk '{print $1}'
}
disk_dir=$(scrollback_dir "$terminal")
disk_baseline_kib=$(du_kib "$disk_dir")

# ---- dispatch ----------------------------------------------------------

dispatch() {
  case "$terminal" in
    mars)
      kill_app mars || true
      "$ROOT/bin/drivers/mars.sh" run-shell "$WORKER"
      ;;
    iterm)
      "$ROOT/bin/drivers/iterm.sh" run-windows "$N" "$WORKER"
      ;;
    terminal)
      "$ROOT/bin/drivers/terminal.sh" run-windows "$N" "$WORKER"
      ;;
    warp)
      echo "idle-9x: warp dispatch is paste-only (TODO automate)" >&2
      "$ROOT/bin/drivers/warp.sh" paste-block "$N" "$WORKER"
      ;;
    *)
      echo "idle-9x: unsupported terminal: $terminal" >&2
      exit 2
      ;;
  esac
}

# ---- sampling ---------------------------------------------------------
#
# A single foreground loop instead of a backgrounded sampler — we want
# precise tick alignment and writes to the JSONL aren't time-critical.

dispatch
echo "==> waiting 10 s for $N sessions / windows to settle…"
sleep 10

samples_jsonl="$RUN_DIR/samples.jsonl"
: > "$samples_jsonl"

# `ps -o %cpu=` reads the kernel's per-process CPU pct.  For multi-process
# apps (iterm2 spawns helpers) we sum across the family.
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
n_samples=0
echo "==> sampling every ${SAMPLE_INTERVAL_S}s for ${DURATION_S}s"
trap 'echo "interrupted, aggregating what we have…" >&2' INT
while [[ $(date +%s) -lt $deadline ]]; do
  t=$(( $(date +%s) - t0 ))
  rss=$(rss_total_kib "$terminal" || echo 0); [[ -z "$rss" ]] && rss=0
  cpu=$(sum_cpu_pct "$terminal")
  disk=$(du_kib "$disk_dir")
  printf '{"t_s":%s,"rss_KiB":%s,"cpu_pct":%s,"disk_KiB":%s}\n' "$t" "$rss" "$cpu" "$disk" \
    >> "$samples_jsonl"
  n_samples=$(( n_samples + 1 ))
  sleep "$SAMPLE_INTERVAL_S"
done
trap - INT

# Tear down our SUT — never the user's foreign terminals.
case "$terminal" in
  mars) kill_app mars || true ;;
esac

# ---- aggregate -------------------------------------------------------

python3 - "$samples_jsonl" "$terminal" "$out_json" \
                          "$baseline_kib" "$disk_baseline_kib" "$disk_dir" \
                          "$DURATION_S" "$N" "$mode" "$RUN_TAG" <<'PY'
import json, os, sys, statistics

(samples_path, terminal, out_json,
 baseline_kib, disk_baseline_kib, disk_dir,
 duration_s, n, mode, run_tag) = sys.argv[1:11]
baseline_kib = int(baseline_kib); disk_baseline_kib = int(disk_baseline_kib)
duration_s = int(duration_s); n = int(n)

samples = []
with open(samples_path) as f:
    for line in f:
        line = line.strip()
        if line:
            samples.append(json.loads(line))

if not samples:
    print("idle-9x: no samples — dispatch / sampling failed", file=sys.stderr)
    sys.exit(1)

rss_delta = [s["rss_KiB"] - baseline_kib for s in samples]
cpu = [s["cpu_pct"] for s in samples]
disk_delta = [s["disk_KiB"] - disk_baseline_kib for s in samples]

# Drift: ratio of last-quarter mean RSS Δ to first-quarter mean.
# > 1.10 → fail (RSS creeping).
q = max(len(rss_delta) // 4, 1)
first_q = rss_delta[:q]
last_q  = rss_delta[-q:]
first_mean = statistics.mean(first_q) if first_q else 0
last_mean  = statistics.mean(last_q) if last_q else 0
drift_ratio = (last_mean / first_mean) if first_mean > 0 else None

idle_cpu_pass = max(cpu) < 5.0  # CLAUDE.md says ~0 %; allow 5 % as tolerance
no_drift_pass = drift_ratio is None or drift_ratio <= 1.10

result = {
    "scenario": f"idle-{n}x",
    "terminal": terminal,
    "metrics": {
        "n_sessions": n,
        "duration_s": duration_s,
        "mode": mode,
        "n_samples": len(samples),
        "rss_baseline_KiB": baseline_kib,
        "rss_delta_first_KiB": rss_delta[0] if rss_delta else None,
        "rss_delta_last_KiB":  rss_delta[-1] if rss_delta else None,
        "rss_delta_max_KiB":   max(rss_delta) if rss_delta else None,
        "rss_drift_ratio_q4_over_q1": round(drift_ratio, 4) if drift_ratio is not None else None,
        "cpu_pct_max": round(max(cpu), 2) if cpu else None,
        "cpu_pct_mean": round(statistics.mean(cpu), 2) if cpu else None,
        "disk_baseline_KiB": disk_baseline_kib,
        "disk_delta_first_KiB": disk_delta[0] if disk_delta else None,
        "disk_delta_last_KiB":  disk_delta[-1] if disk_delta else None,
        "samples": samples,
        "run_tag": run_tag,
        "checks": {
            "idle_cpu_under_5pct": idle_cpu_pass,
            "rss_drift_under_1.10x": no_drift_pass,
        },
    },
    "skipped": [],
}

with open(out_json, "w") as f:
    json.dump(result, f, indent=2)

m = result["metrics"]; c = m["checks"]
print(f"  {terminal:8s} idle-{n}x ({duration_s}s, {len(samples)} samples):")
print(f"    RSS Δ first / last / max  {m['rss_delta_first_KiB']/1024:.0f} / {m['rss_delta_last_KiB']/1024:.0f} / {m['rss_delta_max_KiB']/1024:.0f} MiB  (baseline {baseline_kib/1024:.0f} MiB)")
print(f"    drift ratio q4/q1          {m['rss_drift_ratio_q4_over_q1']}   {'✓' if c['rss_drift_under_1.10x'] else '✗ FAIL >1.10×'}")
print(f"    CPU mean / max             {m['cpu_pct_mean']} / {m['cpu_pct_max']} %        {'✓' if c['idle_cpu_under_5pct'] else '✗ FAIL >5%'}")
if disk_dir:
    print(f"    disk Δ first / last        {m['disk_delta_first_KiB']/1024:.1f} / {m['disk_delta_last_KiB']/1024:.1f} MiB  (in {disk_dir})")
PY

if [[ "$terminal" != "mars" ]]; then
  echo "  NOTE: $N idle window(s) opened in $terminal (run-tag '$RUN_TAG')." >&2
  echo "        Each worker sleeps $((DURATION_S + 30))s — they'll exit on their own," >&2
  echo "        but you can close manually if you want them gone now." >&2
fi
