#!/usr/bin/env bash
#
# The scripts that would start a real agent CLI refuse on a runner
# (bin/_real-agent-guard.sh): bin/record-codex-exchange.sh,
# bin/record-kimi-exchange.sh, bin/test-helpers-real.sh, and
# bin/test-fresh-install.sh with FRESH_VENDOR=1 aimed at a runner.  Each
# refusal is checked to come before anything is fetched or copied.
#
# Where it runs on a runner it also checks the real detection: the
# recorder refuses with no override.  Opens nothing, fetches nothing.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_real-agent-guard.sh"
SB="$(mktemp -d /tmp/marspot-guard.XXXXXX)"
trap 'rm -rf "${SB:?}"' EXIT
mkdir -p "$SB/home"
FAILED=0
fail() { echo "FAIL: $*"; FAILED=1; }
ok() { echo "  ✓ $1"; }

# refused CMD...: exits 2 within 20 s, says why, and leaves no download
# (the scripts fetch into HOME, which points into the scratch directory)
refused() {
  local out rc
  out="$(cd "$SB" && HOME="$SB/home" /usr/bin/timeout 20 "$@" 2>&1)"; rc=$?
  (( rc == 2 )) && grep -q "refusing" <<<"$out" && [[ -z "$(find "$SB" -name '*.tar.gz' -o -name install.sh)" ]] \
    || { echo "    rc=$rc: $out" | head -5; return 1; }
}

: >"$SB/marker"
MARSPOT_RUNNER_MARKERS="$SB/marker" MARSPOT_RUNNER_HOSTS=none refused "$ROOT/bin/record-codex-exchange.sh" "$SB/rec" \
  && ok "a runner marker present: the recorder refuses" || fail "the recorder ran with a runner marker present"
MARSPOT_RUNNER_MARKERS="" MARSPOT_RUNNER_HOSTS="$(hostname -s)" refused "$ROOT/bin/record-codex-exchange.sh" "$SB/rec" \
  && ok "this host named a runner: the recorder refuses" || fail "the recorder ran on a host named a runner"
MARSPOT_RUNNER_MARKERS="" MARSPOT_RUNNER_HOSTS=none runner_reason >/dev/null \
  && fail "no marker and no runner name, yet called a runner" || ok "no marker and no runner name: not a runner"
mkdir -p "$SB/copy"
: >"$SB/copy/.remote-sync"
ROOT="$SB/copy" MARSPOT_RUNNER_HOSTS=none runner_reason >/dev/null \
  && ok "a remote copy (.remote-sync at its top) is a runner" || fail "a remote copy was not called a runner"
is_runner_host mini2 && is_runner_host lx64.local && ! is_runner_host studio \
  && ok "runner host names: mini2 and lx64 are, studio is not" || fail "is_runner_host"
for s in "record-kimi-exchange.sh $SB/kimi $SB/rec" test-helpers-real.sh; do
  # shellcheck disable=SC2086
  MARSPOT_RUNNER_MARKERS="$SB/marker" MARSPOT_RUNNER_HOSTS=none refused "$ROOT/bin/"$s \
    && ok "a runner marker present: ${s%% *} refuses" || fail "${s%% *} ran with a runner marker present"
done
FRESH_VENDOR=1 HOST=mini2 refused "$ROOT/bin/test-fresh-install.sh" "$SB/none.dmg" \
  && ok "FRESH_VENDOR=1 for mini2: test-fresh-install refuses" || fail "test-fresh-install would run the vendors' installers on mini2"

if why="$(runner_reason)"; then
  for s in "record-codex-exchange.sh $SB/rec" "record-kimi-exchange.sh $SB/kimi $SB/rec" test-helpers-real.sh; do
    # shellcheck disable=SC2086
    refused "$ROOT/bin/"$s \
      && ok "on this runner ($why) ${s%% *} refuses with no override" || fail "${s%% *} ran on this runner ($why)"
  done
else
  echo "  - this host is not a runner: the unforced check is left to a run on one"
fi

(( FAILED == 0 )) && echo "test-real-agent-guard: PASS" || echo "test-real-agent-guard: FAIL"
exit "$FAILED"
