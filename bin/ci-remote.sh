#!/usr/bin/env bash
# bin/ci-remote.sh — run what .github/workflows/ci.yml runs, before a push.
#
# The macOS job runs on mini, the Linux one on lx64, at the same time.
# On a pass it records the tree it checked in <git common dir>/ci-pass/, and the
# pre-push hook this script installs refuses to push a commit whose
# tree has no record.  A tree with uncommitted changes is checked but
# not recorded: what was checked would not be what gets pushed.
#
# Each job is stopped after CI_JOB_TIMEOUT seconds (default 3600), and
# Ctrl-C stops both on their hosts, not just the ssh clients here.
#
# Usage:
#   bin/ci-remote.sh          # check HEAD's tree
#   MAC=mini LINUX=lx64 bin/ci-remote.sh
#   bin/ci-remote.sh --job mac|linux   # one job's steps, run on its host

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# The jobs themselves, run on the far side from the synced tree.
job_mac() {
  set -e
  cargo build --workspace --all-targets
  MARSPOT_TEST_JOBS=14 bin/test.sh --no-fail-fast
  cargo clippy --workspace --all-targets -- -D warnings
  bin/lint-deps.sh
  cargo build --release --bins
  # global: the EXIT trap runs after this function's locals are gone
  BUNDLE_DIR="$(mktemp -d)"
  trap 'rm -rf "$BUNDLE_DIR"' EXIT
  bin/build-app-bundle.sh --out "$BUNDLE_DIR/Marspot.app"
  test -x "$BUNDLE_DIR/Marspot.app/Contents/MacOS/marspot-shell"
}

job_linux() {
  set -e
  local s=(-p marspot-term -p marspot-linkify)
  cargo build "${s[@]}" --all-targets
  cargo nextest run "${s[@]}" --no-fail-fast
  cargo clippy "${s[@]}" --all-targets -- -D warnings
}

if [[ ${1:-} == --job ]]; then
  cd "$ROOT"
  case "${2:-}" in
    mac) job_mac ;;
    linux) job_linux ;;
    *) echo "usage: bin/ci-remote.sh --job mac|linux" >&2; exit 2 ;;
  esac
  exit
fi

MAC="${MAC:-mini}"
LINUX="${LINUX:-lx64}"
NAME=ci-marspot
TIMEOUT="${CI_JOB_TIMEOUT:-3600}"
MAX_WAIT="${CI_MAX_WAIT:-3600}"
LOGS="$(mktemp -d)"
RUN_ID="$(date +%H%M%S)$$"
PIDF="/tmp/marspot-ci-$RUN_ID.pgid"

remote_sync() {
  local tool
  tool="$(command -v remote-sync || true)"
  [[ -n $tool ]] || tool="$HOME/workspace/goliajp/golia-claude-configs/bin/remote-sync"
  [[ -x $tool ]] || { echo "remote-sync not found" >&2; exit 1; }
  "$tool" "$@"
}

# The repository's own dir, shared by every worktree: in a worktree
# `.git` is a file, so hooks and records live in the common dir.
GIT_COMMON="$(cd "$ROOT" && git rev-parse --path-format=absolute --git-common-dir)"

install_hook() {
  local hook="$GIT_COMMON/hooks/pre-push"
  # an earlier version of this hook looked under the worktree's .git,
  # which is a file in a worktree; replace it
  [[ -f $hook ]] && grep -q 'ci-remote.sh' "$hook" && grep -q 'git-common-dir' "$hook" && return
  if [[ -e $hook ]] && ! grep -q 'installed by bin/ci-remote.sh' "$hook"; then
    echo "a pre-push hook exists and is not ours: $hook" >&2
    return
  fi
  mkdir -p "$GIT_COMMON/hooks"
  cat > "$hook" <<'HOOK'
#!/usr/bin/env bash
# installed by bin/ci-remote.sh: push only trees that passed it
common="$(git rev-parse --path-format=absolute --git-common-dir)"
while read -r _ local_sha _ _; do
  [[ $local_sha =~ ^0+$ ]] && continue
  tree="$(git rev-parse "$local_sha^{tree}")"
  if [[ ! -f "$common/ci-pass/$tree" ]]; then
    echo "pre-push: $local_sha (tree $tree) has not passed bin/ci-remote.sh" >&2
    exit 1
  fi
done
HOOK
  chmod +x "$hook"
}

# The login shell leads the job's process group; its pid stays written
# down only while the job runs, so stopping it never reaches a group
# that has since been reused.
run_job() {
  local host=$1 job=$2 env=$3 cmd=$4
  ssh "$host" "echo \$\$ > $PIDF; cd ~/work/$NAME && $env && rustc -V && \
    $cmd bin/_bounded.sh $TIMEOUT bin/ci-remote.sh --job $job; rc=\$?; rm -f $PIDF; exit \$rc" </dev/null
}

stop_job() {
  ssh -o ConnectTimeout=5 "$1" "
    if [ -f $PIDF ]; then kill -TERM -- -\$(cat $PIDF) 2>/dev/null; rm -f $PIDF; fi" \
    </dev/null >/dev/null 2>&1 || true
}

mac="" linux=""
interrupted() {
  echo "==> interrupted; stopping the jobs on $MAC and $LINUX"
  [[ -n $mac ]] && { stop_job "$MAC"; kill "$mac" 2>/dev/null; }
  [[ -n $linux ]] && { stop_job "$LINUX"; kill "$linux" 2>/dev/null; }
  exit 130
}
trap interrupted INT TERM

cd "$ROOT"
install_hook
clean=1
[[ -z "$(git status --porcelain)" ]] || clean=0
tree="$(git rev-parse 'HEAD^{tree}')"

remote_sync "$MAC" --name "$NAME" >/dev/null
remote_sync "$LINUX" --name "$NAME" >/dev/null

echo "==> macOS job on $MAC, Linux job on $LINUX"
run_job "$MAC" mac 'export PATH=$HOME/.cargo/bin:/usr/local/bin:$PATH' \
  "/usr/local/bin/bench-lock heavy --max-wait $MAX_WAIT" > "$LOGS/mac.log" 2>&1 &
mac=$!
run_job "$LINUX" linux 'source ~/.cargo/env' "nice -n 10" > "$LOGS/linux.log" 2>&1 &
linux=$!

rc=0
wait $mac || { rc=1; echo "==> macOS job FAILED:"; tail -40 "$LOGS/mac.log"; }
wait $linux || { rc=1; echo "==> Linux job FAILED:"; tail -40 "$LOGS/linux.log"; }
mac="" linux=""
echo "    logs: $LOGS"
(( rc == 0 )) || exit 1

if (( clean )); then
  mkdir -p "$GIT_COMMON/ci-pass"
  touch "$GIT_COMMON/ci-pass/$tree"
  echo "==> passed; tree $tree may be pushed"
else
  echo "==> passed, but the working tree has uncommitted changes: nothing recorded"
fi
