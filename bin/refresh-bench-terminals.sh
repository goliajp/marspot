#!/usr/bin/env bash
# bin/refresh-bench-terminals.sh — one-shot competitors_snapshot refresh.
#
# Pipeline:
#   1. ssh trigger the LaunchAgent on the bench host
#      (com.marspot.bench-trigger; installed via
#      bin/install-bench-launchagent.sh).
#   2. Receiver runs `brew upgrade --cask iterm2 warp ghostty` so the
#      measure is against latest stable, then drives the 4 terminals
#      through bin/_remote-measure-others-mini.sh.
#   3. Poll for the done sentinel (cap 10 min).
#   4. scp the result back, merge per-terminal into bench/baseline.json
#      (preserves entries the receiver didn't refresh — e.g. Warp when
#      its TCC-blocked driver times out).
#   5. Optionally run bench-remote --full to re-verify the gate.
#
# Usage:
#   bin/refresh-bench-terminals.sh                # full pipeline
#   bin/refresh-bench-terminals.sh --no-gate      # skip bench-remote
#   HOST=other-mini bin/refresh-bench-terminals.sh
#
# Per-terminal entries in baseline include version + captured_at +
# bundle_id + method (auto-populated by the receiver), so a drifting
# snapshot (one terminal stuck on an old date) is auditable at a
# glance from baseline.json alone.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

HOST="${HOST:-mini}"
DO_GATE=1
for arg in "$@"; do
  case "$arg" in
    --no-gate) DO_GATE=0 ;;
    -h|--help)
      sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

TS="$(date -u +%Y%m%dT%H%M%SZ)"
OUT_DIR="$ROOT/bench/remote-runs/refresh-$TS"
mkdir -p "$OUT_DIR"
BASELINE="$ROOT/bench/baseline.json"

if ! command -v jq >/dev/null 2>&1; then
  echo "==> jq required (brew install jq)" >&2
  exit 2
fi

# ---- trigger -------------------------------------------------------
echo "==> triggering LaunchAgent on $HOST"
ssh "$HOST" '
  rm -f ~/.marspot-bench-trigger/done \
        ~/.marspot-bench-trigger/result.json \
        ~/.marspot-bench-trigger/err.log
  touch ~/.marspot-bench-trigger/refresh-others.req
'

# ---- wait for done sentinel ---------------------------------------
# Receiver runs brew upgrade (~few sec idle, up to 2 min if updates) +
# 5 terminals (iterm/warp/ghostty/terminal/marspot) sequentially, each
# ~90 s measure + 3 s cooldown ≈ 8 min real work. 15 min gives 2× head-
# room; a hang past that is a bug worth investigating.
echo "==> waiting for done sentinel (cap 15 min)"
deadline=$(( $(date +%s) + 900 ))
while (( $(date +%s) < deadline )); do
  if ssh -o ConnectTimeout=5 "$HOST" '[[ -f ~/.marspot-bench-trigger/done ]]' 2>/dev/null; then
    break
  fi
  sleep 5
done
if ! ssh -o ConnectTimeout=5 "$HOST" '[[ -f ~/.marspot-bench-trigger/done ]]' 2>/dev/null; then
  echo "==> timed out — bench host may be stuck. Check ~/.marspot-bench-trigger/receiver.log on $HOST" >&2
  exit 3
fi

# ---- fetch + validate ----------------------------------------------
echo "==> fetching result + log to $OUT_DIR"
scp -q "$HOST:.marspot-bench-trigger/result.json" "$OUT_DIR/measured.json"
scp -q "$HOST:.marspot-bench-trigger/err.log"     "$OUT_DIR/err.log"

if ! jq empty "$OUT_DIR/measured.json" 2>/dev/null; then
  echo "==> result.json invalid JSON:" >&2
  cat "$OUT_DIR/measured.json" >&2
  echo "==> err.log:" >&2
  cat "$OUT_DIR/err.log" >&2
  exit 4
fi

echo "==> measured terminals: $(jq -r 'keys | map(select(. != "host")) | join(", ")' "$OUT_DIR/measured.json")"
jq . "$OUT_DIR/measured.json"

# ---- merge into baseline -------------------------------------------
echo "==> merging into bench/baseline.json"
python3 - "$BASELINE" "$OUT_DIR/measured.json" "$BASELINE.tmp" <<'PY'
import json, sys
baseline_path, meas_path, tmp_path = sys.argv[1:4]
baseline = json.load(open(baseline_path))
meas = json.load(open(meas_path))
cs = baseline.setdefault("competitors_snapshot", {})
# Per-key full replace — measured has host + each terminal as a
# self-contained dict (version, bundle_id, captured_at, method,
# *_MBps). Replacing wholesale means each refresh updates the entry
# with current metadata; terminals NOT in measured are untouched, so
# a Warp-timeout run still leaves the previous Warp snapshot in
# place untouched (only its captured_at stays at the prior date).
for k, v in meas.items():
    cs[k] = v
# Bump top-level captured_at to the most recent per-entry refresh.
dates = [v.get("captured_at") for v in cs.values()
         if isinstance(v, dict) and v.get("captured_at")]
if dates:
    cs["captured_at"] = max(dates)
with open(tmp_path, "w") as f:
    json.dump(baseline, f, indent=2)
    f.write("\n")
PY
mv "$BASELINE.tmp" "$BASELINE"
echo "==> baseline.json updated."

# Show per-terminal version + date so drift is visible.
python3 - "$BASELINE" <<'PY'
import json, sys
b = json.load(open(sys.argv[1]))
cs = b["competitors_snapshot"]
print("\n  per-terminal snapshot state:")
for k in ("iterm2", "warp", "ghostty", "terminal"):
    v = cs.get(k, {})
    print(f"    {k:9s}  {v.get('version','-'):60s}  {v.get('captured_at','-')}")
PY

# ---- (optional) re-verify gate ------------------------------------
if (( DO_GATE == 1 )); then
  echo
  echo "==> bench-remote --full to re-verify gate (this rebuilds marspot too)"
  bin/bench-remote.sh --full
fi

echo
echo "==> done. Run artefacts: $OUT_DIR"
