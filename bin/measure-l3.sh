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

# No shelld here: RFC-003 retired L4, and the probe spawns the L3 it
# measures in a sandbox of its own.  The two lines that used to resolve
# a shelld binary are gone with it — `marspot_bin marspot-shelld` now
# names a file that is never built, which is exactly the kind of
# leftover that kept this script "running" while every trial failed.

SCENARIOS_DIR="$ROOT/bench/scenarios"
RESULTS_DIR="$ROOT/bench/results"
SESSION_BIN="$(marspot_bin marspot-session)"
PROBE_BIN="$(dirname "$SESSION_BIN")/examples/l3_throughput"
OUT_JSON="$RESULTS_DIR/l3-throughput.json"
mkdir -p "$RESULTS_DIR"

SCENARIOS=(cat-ascii cat-mixed cat-cjk cat-emoji)
# Five, not three.  The probe keeps the fastest trial (load can only
# add time), so trials are lottery tickets for an uncontended window on
# a host that has a permanent tenant.  Measured 2026-08-19: with three,
# one batch came back 28.7 / 72.3 / 160.0 MiB/s for the same scenario —
# it took all three to find one clean run — and the next batch missed
# entirely on cjk (127.8 vs the 162-172 the same build reaches when it
# gets a window).  Two more tickets cost ~50 % of this script's wall
# clock and buy back a metric that otherwise reports the neighbours.
TRIALS="${TRIALS:-5}"
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

# Between trials, make sure no session from a previous one is still
# holding a pty.  The probe sandboxes itself per run, so this is only
# about orphans from an aborted trial.
reset_sessions() {
  pkill -9 -f "$SESSION_BIN( |\$)" >/dev/null 2>&1 || true
  sleep 0.3
}

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
      # One retry, in case an orphaned session held the pty.
      echo "    probe failed — clearing orphans and retrying" >&2
      reset_sessions
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
# Record what the host was doing, so a consumer can tell "marspot got
# slower" from "the bench host was busy".  Without it a contended run
# is indistinguishable from a regression, and this host has a permanent
# tenant.
try:
    load1 = os.getloadavg()[0]
except OSError:
    load1 = -1.0
out = {"_host_load1": round(load1, 2)}
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
    # MINIMUM, not median.  Load can only ADD time, so the fastest
    # trial is the least-contaminated one, while a median still carries
    # whatever else the host was doing.  docs/bench.md §2b argues this
    # for A/B comparisons; it matters at least as much here, because
    # this host has a permanent tenant — the same build read 194.5 and
    # 106.5 MB/s on ascii within one afternoon, and a median of three
    # trials taken during the bad half reports the tenant, not marspot.
    #
    # `median_ns` keeps its name because bench.sh and the JSON's
    # consumers read that key; the value is now the best trial.  Every
    # sample is still recorded, so a wide spread stays visible instead
    # of being averaged into a plausible-looking number.
    best = samples[0]
    bps = total_bytes * 1_000_000_000 // best if best > 0 else 0
    out[scenario] = {
        "bytes": total_bytes,
        "median_ns": best,
        "stat": "min-of-%d" % len(samples),
        "bytes_per_sec": bps,
        "samples": samples,
    }
json.dump(out, open(out_json, "w"), indent=2)
PY

echo
echo "==> $OUT_JSON"
cat "$OUT_JSON"
