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

# Ensure shelld is alive — every test boots a shell+core pair that
# needs a daemon to attach to.  If the LaunchAgent isn't bootstrapped
# (fresh dev box), bootstrap it now.
ensure_shelld() {
  if pgrep -fl marspot-shelld >/dev/null 2>&1; then
    return 0
  fi
  local plist="$HOME/Library/LaunchAgents/com.marspot.shelld.plist"
  if [[ -f "$plist" ]]; then
    launchctl bootstrap "gui/$(id -u)" "$plist" 2>/dev/null || true
    sleep 1
  fi
  pgrep -fl marspot-shelld >/dev/null 2>&1 || {
    echo "shelld not running.  Run \`bin/install-shelld.sh\` first."
    exit 1
  }
}

# Wipe state shared across tests so the next one starts clean.
reset_state() {
  rm -rf "$HOME/Library/Caches/marspot/binaries"
  : > "$HOME/Library/Logs/Marspot/supervisor.log" 2>/dev/null || true
  pkill -9 -f '/marspot-shell( |$)' >/dev/null 2>&1 || true
  pkill -9 -f '/marspot-core( |$)'  >/dev/null 2>&1 || true
  sleep 0.5
}

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
