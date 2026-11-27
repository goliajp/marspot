#!/usr/bin/env bash
# bin/remote-measure-others.sh — automate cross-terminal measurement
# on the bench host (marspot_host bench in bin/_lib.sh), then
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
#   bin/remote-measure-others.sh                # HOST=<host> overrides
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
#     `bin/_remote-measure-others-runner.sh > /tmp/measured.json`
#     yourself; then `scp <host>:/tmp/measured.json -` into a manual
#     baseline merge.
#   - Warp's driver is keystroke-based; if it misfires the marker
#     wait will time out per-terminal and that terminal is omitted
#     from the snapshot (others still update).

set -euo pipefail

# One tool does the copying, for every session and every repo: it takes
# the exclude list from git itself, so a path this repo ignores is never
# sent and never deleted on the far side -- which is how the remote
# target/ survives a sync from a tree that has none.
remote_sync() {
  local tool
  tool="$(command -v remote-sync || true)"
  [[ -n $tool ]] || tool="$HOME/workspace/goliajp/golia-claude-configs/bin/remote-sync"
  [[ -x $tool ]] || {
    echo "remote-sync not found (looked on PATH and in $tool)" >&2
    exit 1
  }
  "$tool" "$@"
}
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"
# shellcheck source=bin/_lock-mark.sh
source "$ROOT/bin/_lock-mark.sh"
HOST="${HOST:-$(marspot_host bench)}"
REMOTE_NAME="${REMOTE_NAME:-bench-marspot}"
# remote-sync always lands in ~/work/<name>, so that is where the
# run has to look.
REMOTE_DIR="work/$REMOTE_NAME"
LOCK_LOCAL="/tmp/marspot-remote-measure.lock.d"
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
  rm -rf "$LOCK_LOCAL"
  exit $rc
}
trap cleanup EXIT INT TERM

# ---- pre-flight ------------------------------------------------------
# No lock of our own on the host: the measurement below holds the host's
# bench lock, which is what keeps every other measurement and heavy job
# off the machine while it runs.
ssh -o ConnectTimeout=5 "$HOST" "mkdir -p ~/$REMOTE_DIR" </dev/null

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
  echo "  2. cd ~/$REMOTE_DIR && bash bin/_remote-measure-others-runner.sh > /tmp/measured.json" >&2
  echo "  3. scp $HOST:/tmp/measured.json $OUT_DIR/measured.json" >&2
  echo "  4. Re-run this script with REMOTE_MEAS_JSON=$OUT_DIR/measured.json to merge into baseline." >&2
  exit 3
fi

# ---- sync ------------------------------------------------------------
GIT_SHA="$("$ROOT/bin/_git-sha.sh")"
remote_sync "$HOST" --name "$REMOTE_NAME"
# read again after the sync: a checkout that moved in between is dirty
GIT_SHA="$("$ROOT/bin/_git-sha.sh" "$GIT_SHA")"

# ---- run + emit JSON to stdout, capture locally ----------------------
mkdir -p "$OUT_DIR"
MEAS_JSON="${REMOTE_MEAS_JSON:-$OUT_DIR/measured.json}"

if [[ -z "${REMOTE_MEAS_JSON:-}" ]]; then
  echo "==> measuring on $HOST: the product built from this tree, and the other terminals"
  # Exclusive, like the gate: this is a measurement, and a competitor's
  # throughput read while the machine is busy is not a slower number,
  # it is not a number.  The product's build runs inside the same lock
  # hold, so nothing else gets between the build and the reading.
  COMMIT="$(git -C "$ROOT" rev-parse --short HEAD)"
  [[ "$GIT_SHA" != *-dirty ]] || COMMIT+="-dirty"
  FWD="export MARSPOT_BENCH_COMMIT=$COMMIT MARSPOT_GIT_SHA=$GIT_SHA; "
  [[ -z "${MEASURE_TERMS:-}" ]] || FWD+="export MEASURE_TERMS=$(printf '%q' "$MEASURE_TERMS"); "
  [[ -z "${MEASURE_ROUNDS:-}" ]] || FWD+="export MEASURE_ROUNDS=$(printf '%q' "$MEASURE_ROUNDS"); "
  [[ -z "${MEASURE_MARKER_TIMEOUT_S:-}" ]] || FWD+="export MEASURE_MARKER_TIMEOUT_S=$(printf '%q' "$MEASURE_MARKER_TIMEOUT_S"); "
  [[ -z "${MEASURE_WARP_VIA:-}" ]] || FWD+="export MEASURE_WARP_VIA=$(printf '%q' "$MEASURE_WARP_VIA"); "
  INNER="source ~/.cargo/env; cd ~/$REMOTE_DIR && export CARGO_TARGET_DIR=\$HOME/$REMOTE_DIR/target; $FWD
    /usr/local/bin/bench-lock bench --max-wait ${MARSPOT_BENCH_MAX_WAIT:-1200} \
      $(lock_until "${MARSPOT_MEASURE_TIMEOUT:-3000}" measure-others "$REMOTE_NAME") \
      env MARSPOT_BENCH_LOCKED=1 caffeinate -dims bin/_bounded.sh ${MARSPOT_MEASURE_TIMEOUT:-3000} sh -c \
      'cargo build --release --bin marspot-shell --bin marspot-core --bin marspot-session >&2 \
        && exec bash bin/_remote-measure-others-runner.sh'"
  # The whole run inside the console user's GUI session when the host
  # allows it: every terminal, the product included, is then started
  # the same way, and the AppleScript-driven ones are reachable.
  GUI_UID="${GUI_UID:-501}"
  if ssh "$HOST" "sudo -n launchctl asuser $GUI_UID /usr/bin/true" </dev/null 2>/dev/null; then
    INNER="export MEASURE_METHOD='ssh, run inside the GUI session'; $INNER"
    ssh "$HOST" "sudo -n launchctl asuser $GUI_UID sudo -u \$(id -un $GUI_UID) /bin/bash -lc $(printf '%q' "$INNER")" \
      </dev/null > "$MEAS_JSON"
  else
    ssh "$HOST" "$INNER" </dev/null > "$MEAS_JSON"
  fi
else
  echo "==> using pre-captured $MEAS_JSON (manual workflow)"
fi

if ! jq empty "$MEAS_JSON" 2>/dev/null; then
  echo "remote produced invalid JSON:" >&2
  cat "$MEAS_JSON" >&2
  exit 2
fi
BASELINE="$ROOT/bench/baseline.json"
echo "==> measured:"
jq . "$MEAS_JSON"

# A run that measured nothing must not look like a run that measured
# everything. The merge below keeps terminals this run did not reach --
# which is right when one driver times out, and a lie when they all do:
# it stamps `captured_at` with today, and the gate then prints numbers
# from a fortnight ago as read this morning.
#
# Happened on 2026-10-02: every driver's marker timed out, the script
# exited 0, and the only change to the baseline was the date.
MEASURED_TERMS=$(jq -r '[to_entries[] | select(.key != "host") | select(.value | type == "object") | select([.value | keys[] | select(endswith("_MBps"))] | length > 0) | .key] | length' "$MEAS_JSON")
if [[ "${MEASURED_TERMS:-0}" -eq 0 ]]; then
  echo "==> nothing was measured: no terminal produced a single throughput" >&2
  echo "    number, so $BASELINE is left exactly as it was." >&2
  echo "    Look above for which drivers timed out; after a macOS upgrade" >&2
  echo "    they need their Automation permission granted again on the" >&2
  echo "    bench host." >&2
  exit 4
fi

if [[ "${MEASURE_NO_MERGE:-}" == "1" ]]; then
  echo "==> MEASURE_NO_MERGE=1: $BASELINE left as it was; the readings are in $MEAS_JSON"
  exit 0
fi

# ---- merge into bench/baseline.json ----------------------------------
TMP="$BASELINE.tmp.$$"
python3 - "$BASELINE" "$MEAS_JSON" "$TMP" <<'PY'
import json, sys, datetime
baseline_path, meas_path, tmp_path = sys.argv[1:4]
baseline = json.load(open(baseline_path))
meas = json.load(open(meas_path))
cs = baseline.setdefault("competitors_snapshot", {})
# readings from two machines are not one snapshot: a new machine starts it over
if (cs.get("host") or {}).get("name") != (meas.get("host") or {}).get("name"):
    cs = baseline["competitors_snapshot"] = {k: v for k, v in cs.items() if not isinstance(v, dict)}
# Per-terminal overwrite + bump captured_at. We keep any
# previously-recorded terminals not in this run (e.g. if warp
# timed out, its old numbers stay rather than vanish silently).
for term, scenarios in meas.items():
    # the per-round figures are kept; the raw trial times stay in the
    # run's own measured.json
    for rd in scenarios.get("rounds", []):
        for k in [k for k in rd if k.endswith("_trials_s")]:
            del rd[k]
    cs[term] = scenarios
cs["captured_at"] = datetime.date.today().isoformat()
with open(tmp_path, "w") as f:
    json.dump(baseline, f, indent=2)
    f.write("\n")
PY
mv "$TMP" "$BASELINE"
echo "==> $BASELINE updated: $MEASURED_TERMS terminal(s) measured, captured_at=$(date +%Y-%m-%d)"
