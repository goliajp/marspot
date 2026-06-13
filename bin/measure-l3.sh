#!/usr/bin/env bash
# bin/measure-l3.sh — production-path (shell→core→L3) single-session cat
# throughput, for each cross-terminal cat-* scenario.
#
# Why this exists: bin/measure.sh drives the standalone `mcli` binary (one
# in-process session, no IPC/shm hop), and the marspot number in
# baseline.json's competitors_snapshot was hand-captured via Screen Sharing
# on the pre-L3 app.  Neither is the architecture the product ships: per-
# session L3 (default since 2026-06-13) routes every pane's bytes through
# shelld → marspot-session (parser → grid → shm publish), ~0.90× the in-
# process bulk-cat rate.  This script measures that real path headlessly so
# bin/bench.sh --full can gate the number the user actually experiences.
#
# It runs entirely in the dev sandbox (its own MARSPOT_STATE_DIR + shelld);
# never touches the installed app.  Output: bench/results/l3-throughput.json,
# one entry per scenario with median_ns / bytes_per_sec / per-trial samples.
#
# Each scenario file is `cat`-ed REPEATS times in one command so the drain
# window clears the 10 ms poll / 300 ms plateau resolution floor (a 32 MiB
# file drains in ~0.2 s — too short to time honestly).  Repeating the path
# keeps the content mix identical; bytes credited = REPEATS × file_bytes.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"          # marspot_bin → CARGO_TARGET_DIR-aware paths
# shellcheck source=/dev/null
source "$ROOT/bin/_dev-sandbox.sh"

# Re-point the sandbox shelld at the resolved target dir.  _dev-sandbox.sh
# hardcodes $ROOT/target/release, but bench-remote.sh exports a dedicated
# CARGO_TARGET_DIR on the mini — so the dev binaries live elsewhere there.
# dev_ensure_shelld / dev_kill_shell_core read these globals at call time.
DEV_TARGET="$(dirname "$(marspot_bin marspot-shelld)")"
DEV_SHELLD="$(marspot_bin marspot-shelld)"

SCENARIOS_DIR="$ROOT/bench/scenarios"
RESULTS_DIR="$ROOT/bench/results"
SESSION_BIN="$(marspot_bin marspot-session)"
PROBE_BIN="$(dirname "$SESSION_BIN")/examples/l3_throughput"
OUT_JSON="$RESULTS_DIR/l3-throughput.json"
mkdir -p "$RESULTS_DIR"

SCENARIOS=(cat-ascii cat-mixed cat-cjk cat-emoji)
TRIALS="${TRIALS:-3}"
# Target drain payload per trial.  ~128 MiB → ~0.3-0.9 s window on Apple
# Silicon: long enough that 10 ms polling resolves it to ~1-3 %, short
# enough to keep the whole gate run under a minute.
TARGET_MIB="${TARGET_MIB:-128}"

fail() { echo "measure-l3: $*" >&2; exit 1; }

# Build session bin + probe together (version-skew guard: a stale probe
# would create a grid_shm region an updated marspot-session refuses).
( cd "$ROOT" && cargo build --release -p marspot-session \
    --example l3_throughput 2>&1 | tail -3 )
[[ -x "$SESSION_BIN" ]] || fail "marspot-session not built at $SESSION_BIN"
[[ -x "$PROBE_BIN" ]]   || fail "probe not built at $PROBE_BIN"

# Bring up a sandbox shelld.  The socket occasionally goes stale between
# runs (a prior aborted session left a dead socket file) — the probe then
# fails with "Connection refused".  Reset-and-retry once before giving up.
reset_shelld() {
  dev_stop_shelld
  pkill -9 -f "$SESSION_BIN( |\$)" >/dev/null 2>&1 || true
  rm -f "$DEV_SOCK"
  rm -rf "$MARSPOT_STATE_DIR/sessions"
  sleep 0.4
  dev_ensure_shelld || fail "shelld did not come up after reset"
}
dev_ensure_shelld || reset_shelld

cleanup() { pkill -9 -f "$SESSION_BIN( |\$)" >/dev/null 2>&1 || true; }
trap cleanup EXIT

run_one() {
  # echoes ns on success, empty on failure
  local scenario_path=$1 repeats=$2
  "$PROBE_BIN" "$SESSION_BIN" "$scenario_path" "$repeats" 2>/dev/null
}

RUN_DIR=$(mktemp -d)
trap 'cleanup; rm -rf "$RUN_DIR"' EXIT

for scenario in "${SCENARIOS[@]}"; do
  spath="$SCENARIOS_DIR/$scenario.bin"
  [[ -f "$spath" ]] || { echo "==> missing $spath — run bin/gen-scenarios.sh" >&2; continue; }
  fbytes=$(stat -f%z "$spath")
  # ceil(TARGET_MIB MiB / file size), at least 1
  repeats=$(python3 -c "import math; print(max(1, math.ceil($TARGET_MIB*1048576/$fbytes)))")
  total_bytes=$((fbytes * repeats))

  : > "$RUN_DIR/$scenario.ns"
  for trial in $(seq 1 "$TRIALS"); do
    echo "==> $scenario ×$repeats ($((total_bytes/1048576)) MiB) trial $trial/$TRIALS"
    ns=$(run_one "$spath" "$repeats")
    if [[ -z "$ns" || "$ns" == "0" ]]; then
      # One stale-socket retry, in case the shelld died mid-suite.
      echo "    probe failed — resetting shelld and retrying" >&2
      reset_shelld
      ns=$(run_one "$spath" "$repeats")
    fi
    if [[ -z "$ns" || "$ns" == "0" ]]; then
      echo "    trial failed (no timing)" >&2
      continue
    fi
    echo "$ns" >> "$RUN_DIR/$scenario.ns"
    mibps=$(python3 -c "print(f'{$total_bytes/1048576/($ns/1e9):.1f}')")
    printf "    %.3fs  %s MiB/s\n" "$(python3 -c "print($ns/1e9)")" "$mibps"
  done
  echo "$total_bytes" > "$RUN_DIR/$scenario.bytes"
done

python3 - "$RUN_DIR" "$OUT_JSON" "${SCENARIOS[*]}" <<'PY'
import json, os, sys
run_dir, out_json, scenarios_str = sys.argv[1:4]
out = {}
for scenario in scenarios_str.split():
    ns_path = os.path.join(run_dir, f"{scenario}.ns")
    by_path = os.path.join(run_dir, f"{scenario}.bytes")
    if not (os.path.exists(ns_path) and os.path.exists(by_path)):
        continue
    samples = sorted(int(x) for x in open(ns_path).read().split() if x.strip())
    if not samples:
        out[scenario] = {"bytes": 0, "median_ns": 0, "bytes_per_sec": 0, "samples": []}
        continue
    total_bytes = int(open(by_path).read().strip())
    med = samples[len(samples) // 2]
    bps = total_bytes * 1_000_000_000 // med if med > 0 else 0
    out[scenario] = {
        "bytes": total_bytes,
        "median_ns": med,
        "bytes_per_sec": bps,
        "samples": samples,
    }
json.dump(out, open(out_json, "w"), indent=2)
PY

echo
echo "==> $OUT_JSON"
cat "$OUT_JSON"
