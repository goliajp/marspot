#!/usr/bin/env bash
# bin/ci-remote.sh — run what .github/workflows/ci.yml runs, before a push.
#
# The macOS job runs on mini, the Linux one on lx64, at the same time.
# On a pass it records the tree it checked in .git/ci-pass/, and the
# pre-push hook this script installs refuses to push a commit whose
# tree has no record.  A tree with uncommitted changes is checked but
# not recorded: what was checked would not be what gets pushed.
#
# Usage:
#   bin/ci-remote.sh          # check HEAD's tree
#   MAC=mini LINUX=lx64 bin/ci-remote.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MAC="${MAC:-mini}"
LINUX="${LINUX:-lx64}"
NAME=ci-marspot
LOGS="$(mktemp -d)"

remote_sync() {
  local tool
  tool="$(command -v remote-sync || true)"
  [[ -n $tool ]] || tool="$HOME/workspace/goliajp/golia-claude-configs/bin/remote-sync"
  [[ -x $tool ]] || { echo "remote-sync not found" >&2; exit 1; }
  "$tool" "$@"
}

install_hook() {
  local hook="$ROOT/.git/hooks/pre-push"
  [[ -f $hook ]] && grep -q 'ci-remote.sh' "$hook" && return
  [[ -e $hook ]] && { echo "a pre-push hook exists and is not ours: $hook" >&2; return; }
  cat > "$hook" <<'HOOK'
#!/usr/bin/env bash
# installed by bin/ci-remote.sh: push only trees that passed it
root="$(git rev-parse --show-toplevel)"
while read -r _ local_sha _ _; do
  [[ $local_sha =~ ^0+$ ]] && continue
  tree="$(git rev-parse "$local_sha^{tree}")"
  if [[ ! -f "$root/.git/ci-pass/$tree" ]]; then
    echo "pre-push: $local_sha (tree $tree) has not passed bin/ci-remote.sh" >&2
    exit 1
  fi
done
HOOK
  chmod +x "$hook"
}

cd "$ROOT"
install_hook
clean=1
[[ -z "$(git status --porcelain)" ]] || clean=0
tree="$(git rev-parse 'HEAD^{tree}')"

remote_sync "$MAC" --name "$NAME" >/dev/null
remote_sync "$LINUX" --name "$NAME" >/dev/null

echo "==> macOS job on $MAC, Linux job on $LINUX"
ssh "$MAC" "cd ~/work/$NAME && export PATH=\$HOME/.cargo/bin:/usr/local/bin:\$PATH && rustc -V && \
  exec /usr/local/bin/bench-lock heavy bash -c '
    set -e
    cargo build --workspace --all-targets
    MARSPOT_TEST_JOBS=14 bin/test.sh
    cargo clippy --workspace --all-targets -- -D warnings
    bin/lint-deps.sh
    cargo build --release --bins
    out=\$(mktemp -d)/Marspot.app
    bin/build-app-bundle.sh --out \$out
    test -x \$out/Contents/MacOS/marspot-shell
  '" </dev/null > "$LOGS/mac.log" 2>&1 &
mac=$!
ssh "$LINUX" "cd ~/work/$NAME && source ~/.cargo/env && rustc -V && \
  exec nice -n 10 bash -c '
    set -e
    s=\"-p marspot-term -p marspot-linkify\"
    cargo build \$s --all-targets
    cargo nextest run \$s --no-fail-fast
    cargo clippy \$s --all-targets -- -D warnings
  '" </dev/null > "$LOGS/linux.log" 2>&1 &
linux=$!

rc=0
wait $mac || { rc=1; echo "==> macOS job FAILED:"; tail -40 "$LOGS/mac.log"; }
wait $linux || { rc=1; echo "==> Linux job FAILED:"; tail -40 "$LOGS/linux.log"; }
echo "    logs: $LOGS"
(( rc == 0 )) || exit 1

if (( clean )); then
  mkdir -p "$ROOT/.git/ci-pass"
  touch "$ROOT/.git/ci-pass/$tree"
  echo "==> passed; tree $tree may be pushed"
else
  echo "==> passed, but the working tree has uncommitted changes: nothing recorded"
fi
