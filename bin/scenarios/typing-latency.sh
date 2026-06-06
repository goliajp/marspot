#!/usr/bin/env bash
# bin/scenarios/typing-latency.sh — input → pixel latency, mars-only.
#
# Why this scenario exists: docs/bench.md records typing-latency as a
# mars-internal metric.  No way to measure the same property in iTerm2
# / Warp / Terminal.app without external screen capture or hardware
# camera, so this is purely longitudinal mars-vs-mars regression.
#
# Method:
#   1. Launch mars with MARS_LATENCY=<path> and MARS_PROFILE=<path>.
#      mars's keystroke handler timestamps t0 on key-down and pairs
#      it with the next layer.setContents — recording (t1 - t0) ns
#      into latency_samples.  On Drop, samples are written as a
#      JSON array.  MARS_PROFILE captures render_calls / feed_ns /
#      etc. so we also get fps for the run.
#   2. Wait for the window to appear and focus.
#   3. osascript drives N keystrokes through System Events, paced so
#      each one starts after the previous one's render.
#   4. Cleanly tear mars down so the Drop handler flushes samples.
#   5. Aggregate p50 / p95 / p99 / mean.
#
# Usage:
#   bin/scenarios/typing-latency.sh mars <out-json> [n_keys=200]
# (Other terminals exit code 2 — scenario unsupported.)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"

terminal=${1:?usage: typing-latency.sh <terminal> <out-json> [n_keys]}
out_json=${2:?usage: typing-latency.sh <terminal> <out-json> [n_keys]}
N_KEYS=${3:-200}

if [[ "$terminal" != "mars" ]]; then
  echo "typing-latency: unsupported on $terminal — instrumented in mars only." >&2
  cat > "$out_json" <<EOF
{"scenario":"typing-latency","terminal":"$terminal","metrics":{},"skipped":["unsupported — mars-only"]}
EOF
  exit 2
fi

RUN_DIR="$MARKER_PREFIX/typing-latency-$$"
rm -rf "$RUN_DIR"; mkdir -p "$RUN_DIR"
LAT_PATH="$RUN_DIR/latency.json"
PROF_PATH="$RUN_DIR/profile.json"
RUN_TAG="mars-bench-typing-$$"

# Worker: sleep just long enough for the keystroke loop to finish,
# then exit.  When all 9 mars sessions exit, mars exits naturally
# (event_loop.exit), Drop runs, MARS_LATENCY/MARS_PROFILE files are
# written.  This is the only reliable teardown path — Rust on macOS
# doesn't run Drop on SIGTERM, and mars consumes Cmd-Q in its input
# handler so System Events keystroke can't close the window.
SLEEP_S=$(( N_KEYS / 30 + 6 ))   # 30-ms cadence + 6 s slack
WORKER="$RUN_DIR/worker.sh"
cat > "$WORKER" <<EOF
#!/bin/sh
printf '\033]0;%s\007' "$RUN_TAG"
sleep $SLEEP_S
exit 0
EOF
chmod +x "$WORKER"

# typing-latency unavoidably needs to steal focus while keystrokes
# are being driven (System Events keystroke → frontmost process).
# Capture the user's app first; restore it at the end.  Trap also
# restores in case of Ctrl-C.
USER_APP=$(current_frontmost_app)
trap 'restore_focus_to "$USER_APP" || true' EXIT INT TERM

kill_app mars || true; kill_app mcli || true
sleep 0.3

# Launch mars with both instrumentation paths set.
MARS_BIN_PATH="$(mars_bin mars)"
( cd "$ROOT" && MARS_LATENCY="$LAT_PATH" MARS_PROFILE="$PROF_PATH" \
  MARS_SHELL="$WORKER" \
  nohup "$MARS_BIN_PATH" > /dev/null 2>&1 < /dev/null & ) || true
disown 2>/dev/null || true

# Wait for mars to come up + accept focus.
echo "==> waiting for mars window…"
for _ in $(seq 1 30); do
  if [[ -n "$(pids_of mars)" ]]; then break; fi
  sleep 0.2
done

mars_pid=$(pids_of mars | head -1)
if [[ -z "$mars_pid" ]]; then
  echo "typing-latency: mars did not start" >&2
  exit 1
fi
sleep 0.6  # let the renderer reach steady state

# Force-focus mars (Front-most via System Events).
#
# SAFETY: System Events keystroke is a *global* input event — it goes
# to whichever app is frontmost at the moment the keystroke fires.
# If mars loses focus mid-loop (user clicks away, another app
# auto-focuses), the keystrokes leak into the user's actual work.
# Two mitigations below:
#   1. Verify mars actually became frontmost before sending any key
#      (abort otherwise — better to skip the metric than corrupt user state)
#   2. Re-check between phases (here and after the keystroke loop)
osascript <<'APPLESCRIPT' >/dev/null 2>&1
tell application "System Events"
  try
    set frontmost of (first process whose unix id is (do shell script "pgrep -al ^mars$ | awk '{print $1}'") as integer) to true
  end try
end tell
APPLESCRIPT
sleep 0.4

frontmost_now=$(osascript -e 'tell application "System Events" to return name of first process whose frontmost is true' 2>/dev/null || echo "")
if [[ "$frontmost_now" != "mars" ]]; then
  echo "typing-latency: mars failed to become frontmost (got '$frontmost_now')" >&2
  echo "                aborting before sending keystrokes — otherwise they would" >&2
  echo "                leak into the user's foreground app." >&2
  cat > "$out_json" <<JSON
{"scenario":"typing-latency","terminal":"mars","metrics":{},"skipped":["mars failed to gain focus; refused to send keystrokes to avoid corrupting user state (frontmost was $frontmost_now)"]}
JSON
  # Wait for workers to exit naturally then return.
  sleep $((SLEEP_S + 5))
  exit 0
fi

echo "==> driving $N_KEYS keystrokes (paced ~30 ms each)"

# Single osascript invocation with a loop — invoking osascript once
# per key adds ~80 ms of process spawn time which would dominate the
# 1–2 ms latency we're trying to measure.
osascript <<APPLESCRIPT >/dev/null
tell application "System Events"
  repeat $N_KEYS times
    keystroke "x"
    delay 0.03
  end repeat
end tell
APPLESCRIPT

# Let final renders settle.
sleep 1.0

# Wait for mars to exit naturally — workers will sleep $SLEEP_S then
# exit, mars exits when all 9 sessions are gone, Drop runs.
echo "==> waiting for mars to exit (all 9 workers sleeping ${SLEEP_S}s)…"
for _ in $(seq 1 $((SLEEP_S * 2 + 30))); do
  [[ -z "$(pids_of mars)" ]] && break
  sleep 0.5
done
if [[ -n "$(pids_of mars)" ]]; then
  echo "typing-latency: mars didn't exit within $((SLEEP_S * 2 + 30))*0.5s — forcing SIGTERM (Drop won't run, samples lost)" >&2
  kill -TERM "$mars_pid" 2>/dev/null || true
  sleep 0.5
  kill -KILL "$mars_pid" 2>/dev/null || true
fi

# ---- aggregate -------------------------------------------------------

trap '{ rm -rf "$RUN_DIR"; restore_focus_to "$USER_APP"; } || true' EXIT INT TERM

python3 - "$LAT_PATH" "$PROF_PATH" "$out_json" "$N_KEYS" "$RUN_TAG" <<'PY'
import json, os, sys, statistics

lat_path, prof_path, out_json, n_keys, run_tag = sys.argv[1:6]
n_keys = int(n_keys)

samples_ns = []
if os.path.exists(lat_path):
    try:
        samples_ns = json.load(open(lat_path))
    except Exception as e:
        print(f"typing-latency: failed to parse {lat_path}: {e}", file=sys.stderr)

prof = {}
if os.path.exists(prof_path):
    try:
        prof = json.load(open(prof_path))
    except Exception:
        pass

def pct(samples, p):
    if not samples: return None
    s = sorted(samples)
    k = max(int(p * len(s)) - 1, 0)
    return s[k]

def fmt_us(ns):
    return None if ns is None else round(ns / 1000.0, 1)

result = {
    "scenario": "typing-latency",
    "terminal": "mars",
    "metrics": {
        "n_keys_attempted": n_keys,
        "n_samples_captured": len(samples_ns),
        "input_latency_us_p50": fmt_us(pct(samples_ns, 0.50)),
        "input_latency_us_p95": fmt_us(pct(samples_ns, 0.95)),
        "input_latency_us_p99": fmt_us(pct(samples_ns, 0.99)),
        "input_latency_us_mean": fmt_us(int(statistics.mean(samples_ns))) if samples_ns else None,
        "input_latency_us_max":  fmt_us(max(samples_ns)) if samples_ns else None,
        "render_calls": prof.get("render_calls"),
        "render_total_us": (prof.get("render_total_ns") or 0) // 1000 if prof else None,
        "render_avg_us":  ((prof.get("render_total_ns") or 0) // (prof.get("render_calls") or 1)) // 1000 if prof else None,
        "feed_total_us":  (prof.get("feed_total_ns") or 0) // 1000 if prof else None,
        "user_events":    prof.get("user_events"),
        "run_tag": run_tag,
    },
    "skipped": [] if samples_ns else ["no latency samples captured — mars may have been SIGKILLed before Drop"],
}

with open(out_json, "w") as f:
    json.dump(result, f, indent=2)

m = result["metrics"]
print(f"  mars     typing-latency (attempted {n_keys} keys, captured {m['n_samples_captured']}):")
if samples_ns:
    print(f"    p50 / p95 / p99   {m['input_latency_us_p50']} / {m['input_latency_us_p95']} / {m['input_latency_us_p99']} µs")
    print(f"    mean / max        {m['input_latency_us_mean']} / {m['input_latency_us_max']} µs")
if prof:
    print(f"    render_calls        {m['render_calls']}")
    print(f"    render avg / total  {m['render_avg_us']} µs / {m['render_total_us']} µs")
PY

exit 0
