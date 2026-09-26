#!/usr/bin/env bash
# bin/test-remote.sh — run bin/test.sh on the remote Apple Silicon host.
#
# Why: the full suite spins up real L3 processes, PTYs and Metal/CoreText
# per test binary.  On the dev box that competes with the terminal the
# user is working in (and once, 2026-07-28, took WindowServer down with
# it); on mini it competes with nothing.  Same arch + macOS, so a pass
# there means what a pass here means.
#
# Usage:
#   bin/test-remote.sh                      # whole suite (default host=mini)
#   bin/test-remote.sh -E 'test(idle)'      # args pass through to nextest
#   HOST=other-mini bin/test-remote.sh
#
# Contract:
#   - exclusive  : mkdir lock locally (atomic; macOS has no flock(1))
#   - separate   : its own remote dir, so it never disturbs the bench
#                  mirror bench-remote.sh keeps in ~/bench-marspot
#   - terminating: Ctrl-C kills the remote process group
#   - honest     : exits with the remote suite's exit code

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOST="${HOST:-mini}"
REMOTE_DIR="${REMOTE_DIR:-test-marspot}"
LOCK_LOCAL="/tmp/marspot-test-remote.lock.d"
REMOTE_PIDF="/tmp/marspot-test-remote.pgid"

if ! mkdir "$LOCK_LOCAL" 2>/dev/null; then
  if [[ -f "$LOCK_LOCAL/pid" ]] && kill -0 "$(cat "$LOCK_LOCAL/pid")" 2>/dev/null; then
    echo "test-remote already running locally (pid $(cat "$LOCK_LOCAL/pid"))" >&2
    exit 1
  fi
  rm -rf "$LOCK_LOCAL" && mkdir "$LOCK_LOCAL"
fi
echo "$$" > "$LOCK_LOCAL/pid"

cleanup() {
  ssh -o ConnectTimeout=5 "$HOST" "
    if [[ -f $REMOTE_PIDF ]]; then
      pgid=\$(cat $REMOTE_PIDF 2>/dev/null || true)
      [[ -n \${pgid:-} ]] && kill -- -\$pgid 2>/dev/null || true
      rm -f $REMOTE_PIDF
    fi" >/dev/null 2>&1 || true
  rm -rf "$LOCK_LOCAL"
}
trap cleanup EXIT INT TERM

# Same excludes as bench-remote.sh: the tree, not its build products.
echo "==> rsync → $HOST:~/$REMOTE_DIR/"
rsync -a --delete --quiet \
  --exclude '/target/' \
  --exclude '/build/' \
  --exclude '/references/*' --include '/references/README.md' \
  --exclude '/bench/remote-runs/' \
  --exclude '/bench/results/' \
  --exclude '/bench/scenarios/*.bin' \
  --exclude '.DS_Store' \
  --exclude-from="$HOME/.config/git/ignore" \
  "$ROOT/" "$HOST:$REMOTE_DIR/"

args=""
for a in "$@"; do args+=" $(printf '%q' "$a")"; done

echo "==> bin/test.sh on $HOST"
set +e
ssh "$HOST" "echo \$\$ > $REMOTE_PIDF; cd ~/$REMOTE_DIR && exec bin/test.sh$args"
rc=$?
set -e
exit $rc
