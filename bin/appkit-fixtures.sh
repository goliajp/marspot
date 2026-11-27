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

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOST="${HOST:-mini}"
REMOTE_NAME="${REMOTE_NAME:-appkit-fixtures-marspot}"
write=0
[[ ${1:-} == --write ]] && write=1

tool="$(command -v remote-sync || true)"
[[ -n $tool ]] || tool="$HOME/workspace/goliajp/golia-claude-configs/bin/remote-sync"
"$tool" "$HOST" --name "$REMOTE_NAME"

env_write=""
(( write )) && env_write="MARSPOT_TEST_WRITE_APPKIT_FIXTURES=1"
ssh "$HOST" "source ~/.cargo/env && cd ~/work/$REMOTE_NAME && $env_write \
  /usr/local/bin/bench-lock heavy --max-wait 3600 bin/_bounded.sh 1800 \
  bin/test.sh --run-ignored only -E 'test(live_appkit)'" </dev/null

if (( write )); then
  scp -q "$HOST:work/$REMOTE_NAME/tests/fixtures/appkit/*" "$ROOT/tests/fixtures/appkit/" </dev/null
  echo "==> recorded into tests/fixtures/appkit/; review with git diff --stat"
fi
