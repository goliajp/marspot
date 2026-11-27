#!/usr/bin/env bash
# bin/remote-measure-multi.sh — N terminal windows flooding at once, the
# product against the terminals that can be driven that way, measured
# on the bench host.
#
#   bin/remote-measure-multi.sh 2 4 9        # one lock hold per count
#   MEASURE_TERMS="ghostty marspot" MEASURE_ROUNDS=4 bin/remote-measure-multi.sh 4
#
# Each count is a hold of the host's bench lock of its own: the build
# and the measurement inside it, as bin/remote-measure-others.sh does.
# Results: bench/remote-runs/multi-<UTC>/windows-<N>.json, one object
# per count from bin/_remote-measure-multi-runner.sh, and a table of them
# on stdout.  Nothing is written to bench/baseline.json.

set -euo pipefail

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
REMOTE_DIR="work/$REMOTE_NAME"
OUT_DIR="$ROOT/bench/remote-runs/multi-$(date -u +%Y%m%dT%H%M%SZ)"
GUI_UID="${GUI_UID:-501}"
(( $# > 0 )) || { sed -n '2,14p' "$0"; exit 2; }
for n in "$@"; do
  [[ $n =~ ^[1-9][0-9]?$ ]] || { echo "a window count, not '$n'" >&2; exit 2; }
done

GIT_SHA="$("$ROOT/bin/_git-sha.sh")"
remote_sync "$HOST" --name "$REMOTE_NAME"
# read again after the sync: a checkout that moved in between is dirty
GIT_SHA="$("$ROOT/bin/_git-sha.sh" "$GIT_SHA")"
mkdir -p "$OUT_DIR"
COMMIT="$(git -C "$ROOT" rev-parse --short HEAD)"
[[ "$GIT_SHA" != *-dirty ]] || COMMIT+="-dirty"

for n in "$@"; do
  FWD="export MARSPOT_BENCH_COMMIT=$COMMIT MARSPOT_GIT_SHA=$GIT_SHA MULTI_WINDOWS=$n; "
  for v in MEASURE_TERMS MEASURE_ROUNDS MULTI_SLOT_S MULTI_TRIALS MULTI_SCENARIOS; do
    [[ -z "${!v:-}" ]] || FWD+="export $v=$(printf '%q' "${!v}"); "
  done
  INNER="source ~/.cargo/env; cd ~/$REMOTE_DIR && export CARGO_TARGET_DIR=\$HOME/$REMOTE_DIR/target; $FWD
    /usr/local/bin/bench-lock bench --max-wait ${MARSPOT_BENCH_MAX_WAIT:-1200} \
      $(lock_until "${MARSPOT_MEASURE_TIMEOUT:-1500}" measure-multi "$REMOTE_NAME") \
      caffeinate -dims bin/_bounded.sh ${MARSPOT_MEASURE_TIMEOUT:-1500} sh -c \
      'cargo build --release --bin marspot-shell --bin marspot-core --bin marspot-session >&2 \
        && exec bash bin/_remote-measure-multi-runner.sh'"
  echo "==> $n windows on $HOST" >&2
  # inside the GUI session, as the user: the windows need it
  ssh "$HOST" "sudo -n launchctl asuser $GUI_UID sudo -u \$(id -un $GUI_UID) /bin/bash -lc $(printf '%q' "$INNER")" \
    </dev/null > "$OUT_DIR/windows-$n.json"
  jq empty "$OUT_DIR/windows-$n.json" 2>/dev/null || {
    echo "the host produced no result for $n windows:" >&2
    cat "$OUT_DIR/windows-$n.json" >&2
    exit 2
  }
done

python3 - "$OUT_DIR" "$@" <<'PY'
import json, sys
out, counts = sys.argv[1], sys.argv[2:]
for n in counts:
    doc = json.load(open(f"{out}/windows-{n}.json"))
    print(f"{n} windows, {doc['host']['os']}, MB/s of all windows together: median of the rounds (min-max)")
    for term, rec in doc.items():
        if not isinstance(rec, dict) or "rounds" not in rec:
            continue
        cells = []
        for key in sorted(k for k in rec if k.endswith("_MBps")):
            lo, hi = rec[f"{key}_range"]
            cells.append(f"{key.removesuffix('_MBps').removeprefix('cat-')} {rec[key]:.1f} ({lo:.1f}-{hi:.1f})")
        if "cpu_ms_per_128MiB" in rec:
            lo, hi = rec["cpu_ms_per_128MiB_range"]
            cells.append(f"CPU ms per 128 MiB {rec['cpu_ms_per_128MiB']:.0f} ({lo:.0f}-{hi:.0f})")
        print(f"  {term:8} grid {rec.get('grid')}: " + ",  ".join(cells or ["no reading"]))
PY
echo "==> $OUT_DIR"
