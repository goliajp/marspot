#!/usr/bin/env bash
# bin/scenarios/scrollback-1m.sh — 1M-line push, single session.
#
# Why this scenario exists: mars's no.1 value includes "unlimited
# scrollback" — but each terminal makes a different policy choice for
# how much history to retain.  This bench surfaces the trade-off.
#
# Workload: cat 1 000 000 lines (~96 MiB) into one session.  Measure:
#   - push throughput          (drain rate under sustained big load — comparable)
#   - RSS Δ after push          (memory cost of whatever each terminal retained)
#   - disk Δ in scrollback dir  (cost of disk-backed retention, where applicable)
#
# Retention is policy-dependent and not directly comparable today:
#   mars         in-memory ring, 10k-line cap   → RSS small, disk 0
#   iTerm2       unlimited (default profile)    → RSS large
#   Terminal.app ~100k-line cap (default)        → RSS small-medium
#   Warp         unlimited                       → RSS + disk grow
#
# When mars's disk-backed scrollback lands, the same scenario will
# additionally validate "RSS stays small + disk grows ~linearly".
#
# Usage:
#   bin/scenarios/scrollback-1m.sh <terminal> <out-json>

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"

terminal=${1:?usage: scrollback-1m.sh <terminal> <out-json>}
out_json=${2:?usage: scrollback-1m.sh <terminal> <out-json>}

# What process name to sample.  For mars we sample mcli, since this
# scenario uses the single-session binary (see dispatch).
case "$terminal" in
  mars) sample_proc=mcli ;;
  *)    sample_proc=$terminal ;;
esac

SCENARIO_FILE="$SCENARIOS_DIR/scrollback-1m.txt"
N_LINES=1000000

# Generate the scenario on demand (deterministic; gitignored — too big
# to keep in the repo).  ~96 MiB.  Each line tagged with its index so
# a future "scroll back to line 1" test can assert retention.
if [[ ! -f "$SCENARIO_FILE" ]]; then
  echo "==> generating scrollback-1m.txt ($N_LINES lines, ~96 MiB)" >&2
  python3 - "$SCENARIO_FILE" "$N_LINES" <<'PY'
import sys
out, n = sys.argv[1], int(sys.argv[2])
chunk_template = "Line {i:07d}: lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod\n"
with open(out, "w") as f:
    for i in range(n):
        f.write(chunk_template.format(i=i))
PY
fi
BYTES=$(stat -f%z "$SCENARIO_FILE")

RUN_DIR="$MARKER_PREFIX/scrollback-1m-$terminal-$$"
rm -rf "$RUN_DIR"; mkdir -p "$RUN_DIR"
RUN_TAG="mars-bench-$$"

WORKER="$RUN_DIR/worker.sh"
cat > "$WORKER" <<EOF
#!/bin/sh
printf '\033]0;%s\007' "$RUN_TAG"
out="$RUN_DIR/timing-\$\$.txt"
/usr/bin/time -p /bin/cat "$SCENARIO_FILE" 2> "\$out"
exit 0
EOF
chmod +x "$WORKER"

# ---- per-terminal scrollback dir ---------------------------------------
#
# Where each terminal persists scrollback to disk (if it does).  We
# diff `du -sk` before/after to measure disk Δ.  Empty string = no
# disk persistence (Terminal.app).
scrollback_dir() {
  case "$1" in
    mars)     echo "$HOME/.cache/mars/scrollback" ;;  # not implemented yet — directory may not exist
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

baseline_kib=$(rss_total_kib "$sample_proc" || echo 0)
[[ -z "$baseline_kib" ]] && baseline_kib=0
disk_dir=$(scrollback_dir "$terminal")
disk_baseline_kib=$(du_kib "$disk_dir")

# ---- dispatch ----------------------------------------------------------

dispatch() {
  case "$terminal" in
    mars)
      # Use mcli (single-session) — mars auto-spawns 9 and applies the
      # same MARS_SHELL to every one, which would push 9× the bytes.
      kill_app mars || true
      kill_app mcli || true
      "$ROOT/bin/drivers/mars.sh" run-shell-mcli "$WORKER"
      ;;
    iterm)
      "$ROOT/bin/drivers/iterm.sh" run-windows 1 "$WORKER"
      ;;
    terminal)
      "$ROOT/bin/drivers/terminal.sh" run-single "$WORKER"
      ;;
    warp)
      "$ROOT/bin/drivers/warp.sh" run-single "$WORKER"
      ;;
    *)
      echo "scrollback-1m: unsupported terminal: $terminal" >&2
      exit 2
      ;;
  esac
}

# ---- RSS sampler -------------------------------------------------------

RSS_LOG="$RUN_DIR/rss.samples"
: > "$RSS_LOG"
sample_rss() {
  set +e
  while true; do
    local rss; rss=$(rss_total_kib "$sample_proc")
    [[ -n "$rss" ]] && echo "$rss" >> "$RSS_LOG"
    sleep 0.25
  done
}

# ---- run --------------------------------------------------------------

t_start_ns=$(python3 -c "import time;print(int(time.time()*1e9))")
sample_rss & SAMPLER_PID=$!
trap 'kill $SAMPLER_PID 2>/dev/null; rm -rf "$RUN_DIR"' EXIT INT TERM

dispatch

# Wait for the single timing file.  Long timeout — slow terminals
# (iTerm2 with unlimited scrollback) take a while to ingest 96 MiB.
deadline=$(( $(date +%s) + 600 ))
count=0
while [[ $(date +%s) -lt $deadline ]]; do
  count=$(find "$RUN_DIR" -maxdepth 1 -name 'timing-*.txt' 2>/dev/null | wc -l | tr -d ' ')
  if [[ "$count" -ge 1 ]]; then break; fi
  sleep 0.5
done

t_end_ns=$(python3 -c "import time;print(int(time.time()*1e9))")
kill $SAMPLER_PID 2>/dev/null || true
wait $SAMPLER_PID 2>/dev/null || true

# Settle, then sample post-state — scrollback writes lag the cat finish.
sleep 1.0
post_kib=$(rss_total_kib "$sample_proc" || echo 0)
[[ -z "$post_kib" ]] && post_kib=0
disk_post_kib=$(du_kib "$disk_dir")

case "$terminal" in
  mars) kill_app mars || true; kill_app mcli || true ;;
esac

# ---- aggregate --------------------------------------------------------

python3 - "$RUN_DIR" "$terminal" "$out_json" "$BYTES" \
                    "$t_start_ns" "$t_end_ns" \
                    "$baseline_kib" "$post_kib" \
                    "$disk_baseline_kib" "$disk_post_kib" \
                    "$disk_dir" "$N_LINES" "$RUN_TAG" <<'PY'
import json, os, sys, glob

(run_dir, terminal, out_json, bytes_,
 t_start_ns, t_end_ns,
 baseline_kib, post_kib,
 disk_baseline_kib, disk_post_kib,
 disk_dir, n_lines, run_tag) = sys.argv[1:14]
bytes_ = int(bytes_)
t_start_ns, t_end_ns = int(t_start_ns), int(t_end_ns)
baseline_kib, post_kib = int(baseline_kib), int(post_kib)
disk_baseline_kib, disk_post_kib = int(disk_baseline_kib), int(disk_post_kib)
n_lines = int(n_lines)

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

push_ns = per_worker_ns[0] if per_worker_ns else (t_end_ns - t_start_ns)
push_throughput_MBps = bytes_ / max(push_ns, 1) * 1e9 / 1024 / 1024

rss_samples = []
rss_path = os.path.join(run_dir, "rss.samples")
if os.path.exists(rss_path):
    rss_samples = [int(x) for x in open(rss_path).read().split() if x.strip().isdigit()]
rss_delta = [s - baseline_kib for s in rss_samples] if rss_samples else []

result = {
    "scenario": "scrollback-1m",
    "terminal": terminal,
    "metrics": {
        "n_lines_pushed": n_lines,
        "bytes_pushed": bytes_,
        "push_ns": push_ns,
        "push_s": round(push_ns / 1e9, 3),
        "push_throughput_MBps": round(push_throughput_MBps, 1),
        "rss_baseline_KiB": baseline_kib,
        "rss_peak_delta_KiB": max(rss_delta) if rss_delta else None,
        "rss_post_delta_KiB": post_kib - baseline_kib if post_kib else None,
        "disk_dir": disk_dir or None,
        "disk_baseline_KiB": disk_baseline_kib,
        "disk_post_KiB": disk_post_kib,
        "disk_delta_KiB": disk_post_kib - disk_baseline_kib,
        "run_tag": run_tag,
    },
    "skipped": [] if per_worker_ns else ["worker timing missing — push_ns falls back to wall time"],
}

with open(out_json, "w") as f:
    json.dump(result, f, indent=2)

m = result["metrics"]
print(f"  {terminal:8s} scrollback-1m ({n_lines:,} lines, {bytes_/1024/1024:.0f} MiB):")
print(f"    push time              {m['push_s']} s")
print(f"    push throughput        {m['push_throughput_MBps']} MiB/s")
if m['rss_peak_delta_KiB'] is not None:
    post_str = f"{m['rss_post_delta_KiB']/1024:.0f}" if m['rss_post_delta_KiB'] is not None else "n/a (terminal exited)"
    print(f"    RSS Δ peak / post      {m['rss_peak_delta_KiB']/1024:.0f} / {post_str} MiB  (baseline {baseline_kib/1024:.0f} MiB)")
if disk_dir:
    print(f"    disk Δ in {disk_dir}")
    print(f"      baseline {disk_baseline_kib/1024:.1f} MiB → post {disk_post_kib/1024:.1f} MiB  (Δ {m['disk_delta_KiB']/1024:.1f} MiB)")
else:
    print(f"    disk Δ                 n/a (terminal does not persist scrollback to disk)")
PY

if [[ "$terminal" != "mars" ]]; then
  echo "  NOTE: 1 window was opened in $terminal — close it manually if your" >&2
  echo "        profile keeps it after shell exit (tab title '$RUN_TAG')." >&2
fi
