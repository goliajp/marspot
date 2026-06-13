#!/usr/bin/env bash
#
# Run every shell/core regression test in sequence.  Reports a
# unified pass/fail summary at the end.  Each script is independent
# and exits cleanly on failure, but they share state (binary tree,
# supervisor.log, shelld daemon) so we reset between runs.
#
# Optional slow tests are off by default:
#   --soak  60 s RSS soak (default cadence is 10 min)
#   --real  full network pipeline via bin/test-real-update.sh
#           (~90 s; needs the local signing key keys/marspot-update.sec)
#
# Usage:
#   bin/test-all.sh          # boot + crash + budget + update + rollback
#   bin/test-all.sh --soak   # …plus 60 s soak smoke
#   bin/test-all.sh --real   # …plus real release-pipeline e2e

set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

# All tests run in the MARSPOT_STATE_DIR sandbox with their OWN
# shelld — never the installed LaunchAgent.  Building / killing /
# wiping here can't reach the terminal you actually use.
ensure_shelld() {
  dev_ensure_shelld || { echo "could not start sandbox shelld"; exit 1; }
}

# Wipe sandbox state between tests (sandbox tree only).
reset_state() {
  dev_kill_shell_core
  dev_wipe_state
  sleep 0.5
}

# Stop the sandbox shelld when the whole suite finishes.
trap dev_stop_shelld EXIT

run() {
  local script="$1"
  local label="$2"
  echo
  echo "==> $label"
  echo "----------------------------------------------------------------"
  if "$ROOT/bin/$script"; then
    PASSED+=("$label")
  else
    FAILED+=("$label")
  fi
}

ensure_shelld
declare -a PASSED=()
declare -a FAILED=()

reset_state; run test-shell-core.sh    "smoke: boot + crash recovery + budget"
reset_state; run test-update-flow.sh   "silent update happy path"
reset_state; run test-rollback.sh      "silent update rollback (broken binary)"
reset_state; run test-shell-rollback-loop.sh "shell crash-loop auto-rollback"

for arg in "$@"; do
  case "$arg" in
    --soak)
      reset_state
      run "soak-shell-core.sh DURATION_S=60 SAMPLE_S=15" "soak: 60 s RSS check"
      ;;
    --real)
      reset_state
      run test-real-update.sh "real release pipeline (signed feed e2e)"
      ;;
  esac
done

echo
echo "================================================================"
echo "SUMMARY: ${#PASSED[@]} passed, ${#FAILED[@]} failed"
for p in "${PASSED[@]+"${PASSED[@]}"}"; do echo "  ✓ $p"; done
for f in "${FAILED[@]+"${FAILED[@]}"}"; do echo "  ✗ $f"; done

exit "${#FAILED[@]}"
