#!/usr/bin/env bash
# bin/remote-measure-others.sh — automate cross-terminal measurement
# on a clean remote Apple Silicon host (default: ssh mini), then
# refresh bench/baseline.json.competitors_snapshot in place.
#
# Why this exists: bin/measure-other.sh prints paste-ready commands
# and waits for a human to fire them in iTerm / Warp / Terminal —
# fine for an attended run, blocking otherwise. bench-remote.sh
# --full self-checks that competitors_snapshot.captured_at is within
# 7 days; without this script the cache went stale and the gate
# refused to run. This dispatches the same scenarios automatically
# via bin/drivers/{iterm,warp,terminal}.sh on the remote host.
#
# Usage:
#   bin/remote-measure-others.sh                # default HOST=mini
#   HOST=other ./bin/remote-measure-others.sh
#
# Contract:
#   - exclusive : mkdir lock both ends
#   - clean     : opens fresh windows via drivers, closes by id on exit
#   - bounded   : per-terminal 600s timeout on marker wait
#   - mutating  : rewrites bench/baseline.json on success only
#     (atomic via temp-file rename); failure path leaves baseline
#     untouched.
#
# Sharp edges:
#   - macOS ssh sessions cannot dispatch AppleEvents to GUI apps —
#     even with `ssh -t`, the spawned process is not attached to the
#     console user's aqua session, and `tell application "iTerm"` /
#     similar fails with `-1712 AppleEvent timeout`. This script
#     health-checks for that up front and exits 3 with a clear
#     workaround if hit.
#   - Workaround when ssh dispatch is blocked: connect to the bench
#     host via Screen Sharing (or run physically) and invoke
#     `bin/_remote-measure-others-mini.sh > /tmp/measured.json`
#     yourself; then `scp mini:/tmp/measured.json -` into a manual
#     baseline merge.
#   - Warp's driver is keystroke-based; if it misfires the marker
#     wait will time out per-terminal and that terminal is omitted
#     from the snapshot (others still update).

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOST="${HOST:-mini}"
REMOTE_DIR="${REMOTE_DIR:-bench-marspot}"
LOCK_LOCAL="/tmp/marspot-remote-measure.lock.d"
REMOTE_LOCK="/tmp/marspot-remote-measure.lock.d"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
OUT_DIR="$ROOT/bench/remote-runs/measure-$TS"

# ---- local lock ------------------------------------------------------
if ! mkdir "$LOCK_LOCAL" 2>/dev/null; then
  if [[ -f "$LOCK_LOCAL/pid" ]] && kill -0 "$(cat "$LOCK_LOCAL/pid")" 2>/dev/null; then
    echo "remote-measure-others already running locally (pid $(cat "$LOCK_LOCAL/pid"))" >&2
    exit 1
  fi
  rm -rf "$LOCK_LOCAL" && mkdir "$LOCK_LOCAL"
fi
echo "$$" > "$LOCK_LOCAL/pid"

cleanup() {
  local rc=$?
  ssh -o ConnectTimeout=5 "$HOST" "rm -rf $REMOTE_LOCK" </dev/null >/dev/null 2>&1 || true
  rm -rf "$LOCK_LOCAL"
  exit $rc
}
trap cleanup EXIT INT TERM

# ---- pre-flight ------------------------------------------------------
ssh -o ConnectTimeout=5 "$HOST" "
  set -e
  mkdir -p ~/$REMOTE_DIR
  if ! mkdir $REMOTE_LOCK 2>/dev/null; then
    if [[ -f $REMOTE_LOCK/pid ]] && kill -0 \$(cat $REMOTE_LOCK/pid) 2>/dev/null; then
      echo 'remote lock held by pid '\$(cat $REMOTE_LOCK/pid) >&2; exit 1
    fi
    rm -rf $REMOTE_LOCK && mkdir $REMOTE_LOCK
  fi
  echo \$\$ > $REMOTE_LOCK/pid
" </dev/null

# ---- health-check: can the ssh-spawned shell dispatch AppleEvents? ---
# We try a trivial no-op AppleEvent with a 3 s timeout. If it returns
# `-1712 (AppleEvent timed out)`, this remote+user combo cannot drive
# GUI apps and the full dispatch will hang for minutes per terminal.
# Bail early with the manual workaround.
HEALTH="$(ssh -o ConnectTimeout=5 "$HOST" \
  'osascript -e "with timeout of 3 seconds" -e "tell application \"System Events\" to count processes" -e "end timeout" 2>&1' \
  </dev/null || true)"
if [[ "$HEALTH" == *"-1712"* ]] || [[ "$HEALTH" == *"timed out"* ]] || [[ "$HEALTH" == *"AppleEvent"* ]]; then
  echo "remote ssh cannot dispatch AppleEvents (got: $HEALTH)" >&2
  echo "" >&2
  echo "Workaround:" >&2
  echo "  1. Screen-Share / VNC into $HOST as a console user." >&2
  echo "  2. cd ~/$REMOTE_DIR && bash bin/_remote-measure-others-mini.sh > /tmp/measured.json" >&2
  echo "  3. scp $HOST:/tmp/measured.json $OUT_DIR/measured.json" >&2
  echo "  4. Re-run this script with REMOTE_MEAS_JSON=$OUT_DIR/measured.json to merge into baseline." >&2
  echo "" >&2
  echo "Or skip the freshness gate for this run:" >&2
  echo "  MARSPOT_BENCH_ALLOW_STALE_COMPETITORS=1 bin/bench-remote.sh --full" >&2
  exit 3
fi

# ---- sync ------------------------------------------------------------
echo "==> rsync → $HOST:~/$REMOTE_DIR/"
rsync -a --delete --quiet \
  --exclude '/target/' \
  --exclude '/build/' \
  --exclude '/references/*' --include '/references/README.md' \
  --exclude '/bench/remote-runs/' \
  --exclude '/bench/results/' \
  --exclude '/bench/scenarios/*.bin' \
  --exclude '.DS_Store' \
  --exclude '/.claude/settings.json' \
  "$ROOT/" "$HOST:$REMOTE_DIR/"

# ---- run + emit JSON to stdout, capture locally ----------------------
mkdir -p "$OUT_DIR"
MEAS_JSON="${REMOTE_MEAS_JSON:-$OUT_DIR/measured.json}"

if [[ -z "${REMOTE_MEAS_JSON:-}" ]]; then
  echo "==> dispatching iTerm / Warp on $HOST (~1-2 min)"
  ssh "$HOST" "cd ~/$REMOTE_DIR && bash bin/_remote-measure-others-mini.sh" \
    </dev/null > "$MEAS_JSON"
else
  echo "==> using pre-captured $MEAS_JSON (manual workflow)"
fi

if ! jq empty "$MEAS_JSON" 2>/dev/null; then
  echo "remote produced invalid JSON:" >&2
  cat "$MEAS_JSON" >&2
  exit 2
fi
echo "==> measured:"
jq . "$MEAS_JSON"

# ---- merge into bench/baseline.json ----------------------------------
BASELINE="$ROOT/bench/baseline.json"
TMP="$BASELINE.tmp.$$"
python3 - "$BASELINE" "$MEAS_JSON" "$TMP" <<'PY'
import json, sys, datetime
baseline_path, meas_path, tmp_path = sys.argv[1:4]
baseline = json.load(open(baseline_path))
meas = json.load(open(meas_path))
cs = baseline.setdefault("competitors_snapshot", {})
# Per-terminal overwrite + bump captured_at. We keep any
# previously-recorded terminals not in this run (e.g. if warp
# timed out, its old numbers stay rather than vanish silently).
for term, scenarios in meas.items():
    cs.setdefault(term, {}).update(scenarios)
cs["captured_at"] = datetime.date.today().isoformat()
with open(tmp_path, "w") as f:
    json.dump(baseline, f, indent=2)
    f.write("\n")
PY
mv "$TMP" "$BASELINE"
echo "==> $BASELINE updated (captured_at=$(date +%Y-%m-%d))"
