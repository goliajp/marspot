#!/usr/bin/env bash
# bin/measure-shader-warm.sh — what a core that has never run pays to
# compile its shaders, and whether the update path's warm-up takes that
# out of the swap.
#
# The system keeps compiled shaders per binary, so the question needs
# core binaries that have never built theirs.  Give it two:
#
#   WARM_FRESH_A, WARM_FRESH_B   two different never-run marspot-core
#                                builds (e.g. target/release and a
#                                second target dir of the same tree)
#   WARM_FRESH_C, WARM_OLD       optional: a third never-run core, and
#                                a directory holding a shell and a core
#                                from before the warm-up existed, for
#                                an update without it to compare with
#
# Part 1, no window: FRESH_A run twice with --warm-shaders (first
# compile, then the kept result), then a copy of it and the copy
# renamed, which is what staging and promoting do to a file.
#
# Part 2, one sandbox app: an update that stages FRESH_B under this
# tree's shell, reading what the warm-up logged and what the swapped-in
# core then logged for its shader library.
#
# Opens windows and is a measurement:
#   E2E_LOCK=bench bin/test-e2e-remote.sh measure-shader-warm.sh
set -uo pipefail
trap 'exit 1' HUP INT TERM

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
dev_no_dock
SUP_LOG="$MARSPOT_STATE_DIR/logs/marspot.log"
TREE="$MARSPOT_STATE_DIR/binaries"
A="${WARM_FRESH_A:?a never-run marspot-core}"
B="${WARM_FRESH_B:?another never-run marspot-core}"
C="${WARM_FRESH_C:-}"
OLD="${WARM_OLD:-}"
SCRATCH="$MARSPOT_STATE_DIR/warm"

fail() {
  echo "FAIL: $*"
  tail -20 "$SUP_LOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}
trap dev_kill_shell_core EXIT
mkdir -p "$SCRATCH"

echo "==> part 1: library and pipeline microseconds, in order"
echo "    first run of a new binary:   $("$A" --warm-shaders)"
echo "    the same file again:         $("$A" --warm-shaders)"
cp "$A" "$SCRATCH/copy"
echo "    a copy of it (new file):     $("$SCRATCH/copy" --warm-shaders)"
mv "$SCRATCH/copy" "$SCRATCH/renamed"
echo "    the copy renamed:            $("$SCRATCH/renamed" --warm-shaders)"

# one_update LABEL SHELL RUNNING_CORE STAGED_CORE
one_update() {
  local label=$1 shell=$2 running=$3 staged=$4 i
  dev_kill_shell_core
  dev_wipe_state
  rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired"
  mkdir -p "$(dirname "$SUP_LOG")"
  env MARSPOT_CORE_BIN="$running" MARSPOT_SESSION_BIN="$ROOT/target/release/marspot-session" \
    nohup "$shell" >/tmp/marspot-measure-shader-warm.log 2>&1 < /dev/null & dev_record_pid $!
  disown
  for (( i = 0; i < 100; i++ )); do
    grep -q $'\tWINDOW_FIRST_PRESENT\t' "$SUP_LOG" 2>/dev/null && break
    sleep 0.1
  done
  grep -q $'\tWINDOW_FIRST_PRESENT\t' "$SUP_LOG" || fail "$label: the app did not come up"
  mkdir -p "$TREE/pending"
  cp "$staged" "$TREE/pending/marspot-core"
  "$shell" --trigger >/dev/null
  for (( i = 0; i < 900; i++ )); do
    grep -q $'\tUPDATE_SWAP\t' "$SUP_LOG" && break
    sleep 0.1
  done
  grep -q $'\tUPDATE_SWAP\t' "$SUP_LOG" || fail "$label: the update did not complete"
  sleep 1
  echo "==> $label"
  grep -E $'\t(UPDATE_SHADER_WARM|update\.shader_warm\.[a-z_]+)\t' "$SUP_LOG" | cut -f7- | sed 's/^/    warm-up:  /'
  grep $'\tRENDERER_SHADERS\t' "$SUP_LOG" | cut -f5,9- | sed 's/^/    core pid /'
  # from the old core's retirement to the new one's first frame
  python3 - "$SUP_LOG" <<'PY'
import sys
t = {}
for line in open(sys.argv[1], errors="replace"):
    c = line.split("\t")
    if len(c) > 7 and c[6] in ("ACTIVE_SHUTDOWN", "WINDOW_FIRST_FRAME", "UPDATE_APPLY"):
        t.setdefault(c[6], []).append(int(c[1]))
if "ACTIVE_SHUTDOWN" in t and len(t.get("WINDOW_FIRST_FRAME", [])) >= 2:
    print(f"    no core drawing for {t['WINDOW_FIRST_FRAME'][-1] - t['ACTIVE_SHUTDOWN'][0]} ms "
          f"(old core retired -> new core's first frame)")
PY
  dev_kill_shell_core
}

one_update "update with the warm-up" "$ROOT/target/release/marspot-shell" "$A" "$B"
if [[ -n $OLD && -n $C ]]; then
  one_update "update without the warm-up (shell from $OLD)" "$OLD/marspot-shell" "$OLD/marspot-core" "$C"
fi
echo "==> done"
