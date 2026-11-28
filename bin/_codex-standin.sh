# shellcheck shell=bash
# Sourced by the suites that run a Codex: the stand-in in
# bin/codex-standin instead of the real CLI, which never runs on a
# runner.  Do not execute.

CODEX_STANDIN_DIR="$ROOT/bin/codex-standin"
CODEX_STANDIN_TARGET=aarch64-apple-darwin

# codex_standin_build OUT: the compiled `codex` at OUT
codex_standin_build() {
  local out=${1:?out} py
  py="$(command -v python3)" || { echo "no python3 on PATH" >&2; return 1; }
  mkdir -p "$(dirname "$out")"
  cc -O1 -DPYTHON="\"$py\"" -DSTANDIN="\"$CODEX_STANDIN_DIR/standin.py\"" \
    -o "$out" "$CODEX_STANDIN_DIR/launcher.c"
}

# codex_standin_release PACKAGES RELEASE: a release of the standalone
# package under PACKAGES (`…/packages/standalone`), as the installer lays
# one out: releases/<release>-<target>/bin/codex
codex_standin_release() {
  local pkg=${1:?packages} rel=${2:?release}
  codex_standin_build "$pkg/releases/$rel-$CODEX_STANDIN_TARGET/bin/codex"
}

# codex_standin_daemons HOME SB: the pids of the app-servers the stand-in
# started for the codex home HOME, from its pid file, when they still run
# with SB in their command line
codex_standin_daemons() {
  local f=$1/app-server-daemon/daemon.pid p
  p="$(sed -n 's/^{"pid":\([0-9]*\).*/\1/p' "$f" 2>/dev/null)"
  [[ -n "$p" ]] && ps -ww -o command= -p "$p" 2>/dev/null | grep -qF "$2" && echo "$p"
}
