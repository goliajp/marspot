#!/usr/bin/env bash
# bin/_build_then.sh BUILD_SECS SUITE_SECS FEATURES SUITE — build the
# release binaries and run bin/SUITE, for a caller that already holds
# the host's lock.
#
# A measurement takes the lock exclusively.  Built under a separate
# shared hold first, its binaries can wait behind every other queued
# measurement and then the measurement queues again; built inside its
# own hold it waits once.  FEATURES is a cargo feature list, or `-`.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 1
# shellcheck disable=SC1090
source ~/.cargo/env

build_secs=$1 suite_secs=$2 features=$3 suite=$4
[[ $features == - ]] && features=""

bin/_bounded.sh "$build_secs" sh -c "cargo build --release ${features:+--features $features} \
  --bin marspot-shell --bin marspot-core --bin marspot-session && cargo build --release -p marspot-peer" 2>&1 | tail -2
[[ ${PIPESTATUS[0]} == 0 ]] || { echo "FAIL: release build" >&2; exit 1; }
# macOS assesses a freshly built executable on its first exec; pay that
# before the suite's stopwatch starts.
for b in marspot-shell marspot-core marspot-session; do
  MARSPOT_NO_REDIRECT=1 "target/release/$b" --version >/dev/null 2>&1
done
bin/_bounded.sh "$suite_secs" "bin/$suite"
status=$?
# when the last measurement on this host ended, for whoever queues the next
touch /tmp/marspot-bench-last
exit "$status"
