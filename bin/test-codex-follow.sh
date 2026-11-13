#!/usr/bin/env bash
#
# The installed codex CLI follows its daemon, end to end: a codex home
# whose daemon package has selected a newer release than the CLI, a
# sandbox marspot, and codex's real installer fetched and run by the
# plugin.  Afterwards the CLI package selects at least the daemon's
# release and the command on PATH is that release.
#
# Fetches from the network (the installer and a codex package).
# Sandbox-only: MARSPOT_STATE_DIR and HOME are redirected, so the
# installer writes into this test's home and nowhere else.  Opens a
# window: run it through bin/test-e2e-remote.sh.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

OLD=0.163.0
OLD_SHA256=b0344fc2e41f15e0071147e5869008047b8ff4bf2c4a8897975d3bae3f103b34
DAEMON=0.164.0
TARGET=aarch64-apple-darwin

# A scratch directory of this run's own, removed when the run passes:
# the fixed name was shared by every run of the suite on the machine.
SB=/tmp/marspot-cxf.$$
RUNLOG=$SB/run.log
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"

fail() {
  echo "FAIL: $*"
  echo "  --- shell lines about the update:"
  grep -E 'codex\.update' "$APPLOG" 2>/dev/null | tail -20 | cut -c1-260 | sed 's/^/    /'
  echo "  --- packages:"; ls -la "$PKG" "$PKG/releases" 2>/dev/null | sed 's/^/    /'
  exit 1
}
ok() { echo "  ✓ $1"; }
cleanup() {
  local rc=$?
  dev_kill_shell_core
  (( rc != 0 )) || rm -rf "$SB"
}
trap cleanup EXIT

wait_for() { # regex, seconds
  local re="$1" secs="$2" i
  for (( i = 0; i < secs * 5; i++ )); do
    grep -qE "$re" "$APPLOG" 2>/dev/null && return 0
    sleep 0.2
  done
  return 1
}

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" || ! -x "$SESSION_BIN" ]]; then
  echo "==> building release binaries"
  ( cd "$ROOT" && cargo build --release \
      --bin marspot-shell --bin marspot-core --bin marspot-session 2>&1 | tail -3 )
fi

OLD_PKG="$HOME/.cache/marspot-e2e/codex-$OLD"
if [[ ! -x "$OLD_PKG/bin/codex" ]]; then
  rm -rf "$OLD_PKG"; mkdir -p "$OLD_PKG"
  curl -fsSL -o "$OLD_PKG/codex.tar.gz" \
    "https://github.com/openai/codex/releases/download/rust-v$OLD/codex-package-$TARGET.tar.gz" || { echo "download failed"; exit 1; }
  got="$(shasum -a 256 "$OLD_PKG/codex.tar.gz" | cut -d' ' -f1)"
  [[ "$got" == "$OLD_SHA256" ]] || { echo "codex $OLD archive digest $got, expected $OLD_SHA256"; exit 1; }
  tar -xzf "$OLD_PKG/codex.tar.gz" -C "$OLD_PKG" && rm "$OLD_PKG/codex.tar.gz"
fi

# ── this test's own HOME: the CLI on OLD, the daemon on DAEMON ─────
rm -rf "$SB"; mkdir -p "$SB"
export HOME="$SB/home"
PKG="$HOME/.codex/packages/standalone"
mkdir -p "$PKG/releases" "$HOME/.local/bin"
cp -Rc "$OLD_PKG" "$PKG/releases/$OLD-$TARGET"
ln -sfn "$PKG/releases/$OLD-$TARGET" "$PKG/current"
ln -sfn "$PKG/current/bin/codex" "$HOME/.local/bin/codex"
# only the daemon's selection is read; nothing runs from it here
DPKG="$HOME/.codex/packages/app-server-daemon"
mkdir -p "$DPKG/releases/$DAEMON-$TARGET"
ln -sfn "$DPKG/releases/$DAEMON-$TARGET" "$DPKG/current"
export PATH="$HOME/.local/bin:$PATH"
[[ "$(codex --version)" == "codex-cli $OLD" ]] || fail "setup: codex on PATH is $(codex --version 2>&1)"

echo "==> launching"
dev_kill_shell_core
rm -rf "$MARSPOT_STATE_DIR"; mkdir -p "$MARSPOT_STATE_DIR/logs"
MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
disown

wait_for "codex\.update\.install.*cli=$OLD.*daemon=$DAEMON" 90 \
  || fail "the plugin never started the installer"
ok "the plugin saw the cli on $OLD behind the daemon on $DAEMON and ran the installer"
wait_for 'codex\.update\.(installed|install_failed)' 300 || fail "the installer did not finish within 300 s"
grep -q 'codex\.update\.install_failed' "$APPLOG" && fail "the installer failed"
ok "the installer finished"

newer_or_same() { # a b → a >= b, as versions
  [[ "$(printf '%s\n%s\n' "$1" "$2" | sort -t. -k1,1n -k2,2n -k3,3n | tail -1)" == "$1" ]]
}
SEL="$(basename "$(readlink "$PKG/current")")"
SEL="${SEL%%-*}"
newer_or_same "$SEL" "$DAEMON" || fail "the cli package selects $SEL, older than the daemon's $DAEMON"
ok "the cli package now selects $SEL"
ON_PATH="$(codex --version 2>&1)"
[[ "$ON_PATH" == "codex-cli $SEL" ]] || fail "codex on PATH says $ON_PATH, expected $SEL"
ok "codex on PATH is $SEL"

# a minute later the plugin looks again; nothing is behind, nothing runs
sleep 70
(( $(grep -c 'codex\.update\.install[[:space:]]' "$APPLOG") == 1 )) || fail "the installer ran more than once"
ok "it ran once"

echo "PASS — the codex cli is brought up to its daemon's release without anyone running the installer"
