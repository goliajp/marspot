#!/usr/bin/env bash
# bin/scenarios/multi-session-9x.sh — 9 parallel cat workloads.
#
# Why this scenario exists: marspot's no.1 value proposition is hosting
# many concurrent sessions for multi-session Claude-Code work.  A
# benchmark that doesn't measure the multi-session case doesn't
# measure the product.
#
# Workload: each of 9 workers runs `time -p cat bench/scenarios/cat-mixed.bin`.
# 9 × 16 MiB = 144 MiB total cross-PTY traffic.  Each worker writes
# its real-time to a per-worker timing file; we wait for 9 files,
# compute aggregate wall + per-worker median, sample RSS as delta vs.
# the pre-launch baseline so the user's existing windows don't bias
# the measurement.
#
# Cross-terminal safety: marspot (our SUT) is killed and relaunched.
# iterm2 / warp / terminal.app are NEVER killed — quitting them would
# destroy the user's open work.  We only open new windows in those
# apps; if their profile setting is "keep window open after exit",
# the user will need to close those leftover windows themselves.
# A future driver can track per-window IDs and close just ours.
#
# Usage:
#   bin/scenarios/multi-session-9x.sh <terminal> <out-json>
# Exit codes per docs/bench.md.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"

terminal=${1:?usage: multi-session-9x.sh <terminal> <out-json>}
out_json=${2:?usage: multi-session-9x.sh <terminal> <out-json>}

N=9
SCENARIO=cat-mixed
SCENARIO_PATH="$SCENARIOS_DIR/$SCENARIO.bin"
[[ -f "$SCENARIO_PATH" ]] || { "$ROOT/bin/gen-scenarios.sh" >/dev/null; }
BYTES=$(stat -f%z "$SCENARIO_PATH")

RUN_DIR="$MARKER_PREFIX/multi9x-$terminal-$$"
rm -rf "$RUN_DIR"; mkdir -p "$RUN_DIR"

# Worker tags its tab title with this so we can spot leftover windows.
RUN_TAG="marspot-bench-$$"

WORKER="$RUN_DIR/worker.sh"
cat > "$WORKER" <<EOF
#!/bin/sh
# Set a recognisable tab title via OSC-0 — useful for spotting leftover
# windows after the bench, and for window-tracking in future drivers.
printf '\033]0;%s\007' "$RUN_TAG"
out="$RUN_DIR/timing-\$\$.txt"
/usr/bin/time -p /bin/cat "$SCENARIO_PATH" 2> "\$out"
# Exit so terminals whose profile is "close window on exit" tidy up
# automatically.  Profiles set to "keep open" leave the window for the
# user to close — we never force-quit a foreign app.
exit 0
EOF
chmod +x "$WORKER"

# ---- baseline RSS -------------------------------------------------------
#
# Sample RSS BEFORE we open any bench windows.  All later RSS readings
# are reported as delta vs. this baseline, so the user's existing
# iTerm2 / Warp tabs / windows don't count against us.

baseline_kib=$(rss_total_kib "$terminal" || echo 0)
[[ -z "$baseline_kib" ]] && baseline_kib=0

# ---- terminal-specific dispatch -----------------------------------------

WIN_IDS_FILE="$RUN_DIR/window-ids.txt"
dispatch() {
  case "$terminal" in
    marspot)
      # marspot is the SUT and not the user's daily driver in this repo —
      # killing it is fine.  Auto-spawns 9 sessions in 3×3 grid;
      # MARSPOT_SHELL is per-session so all 9 run our worker.sh.
      kill_app marspot || true
      "$ROOT/bin/drivers/marspot.sh" run-shell "$WORKER"
      ;;
    iterm)
      # Open 9 fresh windows; capture their window IDs so we can close
      # ONLY those at the end (no content matching, no collateral
      # damage to the user's existing iTerm2 windows).
      "$ROOT/bin/drivers/iterm.sh" run-windows "$N" "$WORKER" > "$WIN_IDS_FILE"
      ;;
    terminal)
      "$ROOT/bin/drivers/terminal.sh" run-windows "$N" "$WORKER" > "$WIN_IDS_FILE"
      ;;
    warp)
      "$ROOT/bin/drivers/warp.sh" paste-block "$N" "$WORKER"
      ;;
    *)
      echo "multi-session-9x: unsupported terminal: $terminal" >&2
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

# ---- RSS sampler --------------------------------------------------------
#
# Background loop: every 250 ms, sum RSS across all processes whose
# binary path matches the terminal.  We record absolute samples; the
# aggregator turns them into delta vs. baseline.
RSS_LOG="$RUN_DIR/rss.samples"
: > "$RSS_LOG"
sample_rss() {
  set +e
  while true; do
    local rss; rss=$(rss_total_kib "$terminal")
    [[ -n "$rss" ]] && echo "$rss" >> "$RSS_LOG"
    sleep 0.25
  done
}

# ---- run ----------------------------------------------------------------

t_start_ns=$(python3 -c "import time;print(int(time.time()*1e9))")
sample_rss & SAMPLER_PID=$!
# Trap also closes any bench windows we opened — guarantees cleanup
# even on Ctrl-C / abort, not just on the happy path at the end.
trap '{ kill $SAMPLER_PID 2>/dev/null; cleanup_windows; rm -rf "$RUN_DIR"; restore_focus_to "$USER_APP"; } || true' EXIT INT TERM

# Focus isolation: capture the user's foreground app, dispatch (which
# may briefly flash the bench terminal to front), then return focus.
# The bench windows keep running their workload — they don't need
# focus to drain PTY.
USER_APP=$(current_frontmost_app)
dispatch
restore_focus_to "$USER_APP"

# Each worker writes timing-<pid>.txt.  Wait for N such files.
deadline=$(( $(date +%s) + 300 ))
count=0
while [[ $(date +%s) -lt $deadline ]]; do
  count=$(find "$RUN_DIR" -maxdepth 1 -name 'timing-*.txt' 2>/dev/null | wc -l | tr -d ' ')
  if [[ "$count" -ge "$N" ]]; then break; fi
  sleep 0.5
done

t_end_ns=$(python3 -c "import time;print(int(time.time()*1e9))")
kill $SAMPLER_PID 2>/dev/null || true
wait $SAMPLER_PID 2>/dev/null || true

# Final RSS reading - capture state shortly after workers finish.
sleep 0.5
post_kib=$(rss_total_kib "$terminal" || echo 0)
[[ -z "$post_kib" ]] && post_kib=0

# Quit our SUT — never the user's iterm/warp/terminal.
case "$terminal" in
  marspot) kill_app marspot || true ;;
esac

count=$(find "$RUN_DIR" -maxdepth 1 -name 'timing-*.txt' 2>/dev/null | wc -l | tr -d ' ')

# ---- aggregate ----------------------------------------------------------

python3 - "$RUN_DIR" "$terminal" "$out_json" "$N" "$count" "$BYTES" \
                    "$t_start_ns" "$t_end_ns" "$SCENARIO" \
                    "$baseline_kib" "$post_kib" "$RUN_TAG" <<'PY'
import json, os, sys, glob, statistics

(run_dir, terminal, out_json, n, count, bytes_,
 t_start_ns, t_end_ns, scenario,
 baseline_kib, post_kib, run_tag) = sys.argv[1:13]
n = int(n); count = int(count); bytes_ = int(bytes_)
t_start_ns, t_end_ns = int(t_start_ns), int(t_end_ns)
baseline_kib = int(baseline_kib); post_kib = int(post_kib)

per_worker_ns = []
for f in sorted(glob.glob(os.path.join(run_dir, "timing-*.txt"))):
    for line in open(f):
        if line.startswith("real"):
            try:
                s = line.split()[1]
                if "m" in s:
                    mins, rest = s.split("m"); secs = float(rest.rstrip("s"))
                    total = float(mins)*60 + secs
                else:
                    total = float(s)
                per_worker_ns.append(int(total * 1e9))
            except Exception:
                pass
            break

rss_samples = []
rss_path = os.path.join(run_dir, "rss.samples")
if os.path.exists(rss_path):
    rss_samples = [int(x) for x in open(rss_path).read().split() if x.strip().isdigit()]

# Deltas vs. baseline (RSS attributable to the bench scenario, not the
# user's pre-existing terminal state).
rss_delta = [s - baseline_kib for s in rss_samples] if rss_samples else []
rss_peak_delta_kib = max(rss_delta) if rss_delta else None
rss_avg_delta_kib  = int(sum(rss_delta) / len(rss_delta)) if rss_delta else None
rss_post_delta_kib = post_kib - baseline_kib if post_kib else None

wall_ns = max(t_end_ns - t_start_ns, 1)
total_bytes = bytes_ * count
agg_throughput_MBps = total_bytes / wall_ns * 1e9 / 1024 / 1024
def _bps(ns):
    # ns may be 0 if /usr/bin/time -p rounded a sub-10ms run to 0.00.
    # Treat as "1 ns" so we report a finite (huge) number rather than crash.
    return bytes_ / max(ns, 1) * 1e9 / 1024 / 1024
per_worker_MBps_p50 = _bps(statistics.median(per_worker_ns)) if per_worker_ns else 0
per_worker_MBps_p95 = 0
if per_worker_ns:
    sorted_ns = sorted(per_worker_ns)
    k = max(int(0.95 * len(sorted_ns)) - 1, 0)
    per_worker_MBps_p95 = _bps(sorted_ns[k])

result = {
    "scenario": scenario + ".9x",
    "terminal": terminal,
    "metrics": {
        "n_workers_expected": n,
        "n_workers_done": count,
        "wall_ns": wall_ns,
        "wall_s": round(wall_ns / 1e9, 3),
        "aggregate_throughput_MBps": round(agg_throughput_MBps, 1),
        "per_worker_MBps_p50": round(per_worker_MBps_p50, 1),
        "per_worker_MBps_p95": round(per_worker_MBps_p95, 1),
        "per_worker_ns": per_worker_ns,
        "rss_baseline_KiB": baseline_kib,
        "rss_peak_delta_KiB": rss_peak_delta_kib,
        "rss_avg_delta_KiB": rss_avg_delta_kib,
        "rss_post_delta_KiB": rss_post_delta_kib,
        "run_tag": run_tag,
    },
    "skipped": [] if count == n else [f"only {count}/{n} workers completed within timeout"],
}

with open(out_json, "w") as f:
    json.dump(result, f, indent=2)

# Pretty stdout
m = result["metrics"]
print(f"  {terminal:8s} multi-session-9x ({scenario}, n={n}):")
print(f"    workers done           {m['n_workers_done']}/{m['n_workers_expected']}")
print(f"    wall time              {m['wall_s']} s")
print(f"    aggregate throughput   {m['aggregate_throughput_MBps']} MiB/s")
print(f"    per-worker p50/p95     {m['per_worker_MBps_p50']} / {m['per_worker_MBps_p95']} MiB/s")
if m['rss_peak_delta_KiB'] is not None:
    print(f"    RSS Δ peak / avg / post  {m['rss_peak_delta_KiB']/1024:.0f} / {m['rss_avg_delta_KiB']/1024:.0f} / {m['rss_post_delta_KiB']/1024:.0f} MiB  (baseline {baseline_kib/1024:.0f} MiB)")
PY

if [[ "$terminal" != "marspot" && "$terminal" != "warp" ]]; then
  echo "  cleanup: closing $N $terminal window(s) we opened (by tracked IDs)…" >&2
  cleanup_windows
fi

if (( count == 0 )); then
  exit 1
fi
exit 0
