#!/usr/bin/env bash
# bin/appkit-fixtures.sh [--write] — check (or rewrite) what the unit
# tests use in place of AppKit's own answers, on the runner.
#
# Loading a system symbol or asking for a keyboard layout registers the
# asking process with LaunchServices, and the suite runs every test in a
# process of its own: done live, each test that paints the chrome or
# reads a layout was one app registration on the runner.  So the unit
# tests read the icon masks and the layouts' `uchr` tables from
# tests/fixtures/appkit/, recorded from AppKit.  The live checks that
# compare those files with what the system answers now are ignored in
# the suite; this runs them, a handful of registrations in all.
#
#   bin/appkit-fixtures.sh            # the files are what the system answers
#   bin/appkit-fixtures.sh --write    # record them again and copy them back
#
# Run it after a macOS update on the runner, or when a test asks for an
# icon size or a layout that is not recorded (add it first: the sizes in
# src/ui/system/macos/icons/fixture.rs, the layouts in
# src/keyboard_layout/installed.rs).

#
# tests/fixtures/appkit/RECORDED names the machine and the macOS the
# files were recorded on; --write rewrites it, a check prints it next to
# what the runner runs now.  The remote copy is removed at the end.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOST="${HOST:-mini}"
REMOTE_NAME="${REMOTE_NAME:-appkit-fixtures-marspot}"
GUI_UID="${GUI_UID:-501}"
write=0
[[ ${1:-} == --write ]] && write=1
recorded="$ROOT/tests/fixtures/appkit/RECORDED"

# gui_cmd and remote_sync
# shellcheck source=bin/_e2e-remote-lib.sh
source "$ROOT/bin/_e2e-remote-lib.sh"

remote_sync "$HOST" --name "$REMOTE_NAME"
trap 'remote_sync "$HOST" --name "$REMOTE_NAME" --forget || true' EXIT

if (( ! write )); then
  echo "==> the files were recorded on:"
  sed 's/^/    /' "$recorded"
  echo "==> $HOST runs now:"
  ssh -n "$HOST" 'printf "host: %s\n" "$(hostname -s)"; sw_vers' | sed 's/^/    /'
fi

# in the console login session: over a plain ssh the Text Input Sources
# list and LaunchServices may be empty, and the check would compare the
# files with nothing
env_write=""
(( write )) && env_write="MARSPOT_TEST_WRITE_APPKIT_FIXTURES=1"
ssh "$HOST" "$(gui_cmd "source ~/.cargo/env && cd ~/work/$REMOTE_NAME && $env_write \
  /usr/local/bin/bench-lock heavy --max-wait 3600 bin/_bounded.sh 1800 \
  bin/test.sh --run-ignored only -E 'test(live_appkit)'")" </dev/null

if (( write )); then
  scp -q "$HOST:work/$REMOTE_NAME/tests/fixtures/appkit/*" "$ROOT/tests/fixtures/appkit/" </dev/null
  ssh -n "$HOST" 'printf "host: %s\n" "$(hostname -s)"; sw_vers' >"$recorded"
  echo "==> recorded into tests/fixtures/appkit/; review with git diff --stat"
fi
