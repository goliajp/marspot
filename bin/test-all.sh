#!/usr/bin/env bash
#
# Run every shell/core regression suite, on the runner.
#
# These suites open real sandbox windows, so they run on the remote Mac
# and never on the machine someone is working on: this hands the list to
# bin/test-e2e-remote.sh, which gives each suite its own state dir and a
# time limit, and prints a pass/fail summary.
#
#   bin/test-all.sh          # every suite test-e2e-remote runs by default
#   bin/test-all.sh --soak   # …plus a 60 s run of soak-shell-core.sh
#
# Not run from here: test-real-update.sh, test-negative-update.sh and
# test-adversarial-update.sh sign a feed with the private update key,
# which stays on the developer's machine, and they open windows, which
# must not happen there.  They are run by hand until that is settled.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

suites=()
for arg in "$@"; do
  case "$arg" in
    --soak) suites+=(soak-shell-core.sh) ;;
    *) echo "usage: bin/test-all.sh [--soak]" >&2; exit 2 ;;
  esac
done

if (( ${#suites[@]} == 0 )); then
  exec "$ROOT/bin/test-e2e-remote.sh"
fi
# The default set first, then the extras.  Reading the default from the
# runner keeps one list.
default_suites() {
  sed -n '/^DEFAULT_SUITES=(/,/^)/p' "$ROOT/bin/test-e2e-remote.sh" | sed '1d;$d' | tr -d ' '
}
all=()
while IFS= read -r s; do [[ -n $s ]] && all+=("$s"); done < <(default_suites)
E2E_ENV="DURATION_S=60 SAMPLE_S=15" exec "$ROOT/bin/test-e2e-remote.sh" "${all[@]}" "${suites[@]}"
