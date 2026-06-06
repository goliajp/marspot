#!/usr/bin/env bash
# bin/drivers/ghostty.sh — Ghostty launcher.
#
# Ghostty 1.3.x has no public AppleScript dictionary, but the binary
# accepts `-e <executable>` (xterm-style) to spawn a fresh surface that
# runs the given command and dies. That's the cleanest path for bench:
# no AppleScript, no System Events keystroke (SE is blocked under
# launchctl-asuser / bsexec ssh sessions anyway — TCC Accessibility
# boundary).
#
# Usage:
#   bin/drivers/ghostty.sh run-single <cmd>
#     Spawn a fresh Ghostty surface running <cmd>. <cmd> is the shell
#     string the surface should execute — we wrap it in a temp script
#     because Ghostty's -e wants an executable path, not an inline
#     bash string.
#
#   bin/drivers/ghostty.sh quit (refuses)
#
# Note on ssh+ssh-driven runs: the caller (e.g.
# bin/_remote-measure-others-mini.sh) wraps this driver in
# `sudo -n launchctl asuser <uid>` so the spawned ghostty lives in the
# user's GUI launchd domain. Without that, ssh-spawned ghostty has no
# Aqua bootstrap and its surface never materializes. The wrapper is
# transparent to this driver.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"

GHOSTTY_BIN="/Applications/Ghostty.app/Contents/MacOS/ghostty"

cmd_word=${1:-}
shift || true

case "$cmd_word" in
  run-single)
    user_cmd=${1:?cmd}; shift || true
    # Write the user command to a temp script so we can pass an
    # executable path to `-e`. Ghostty does not accept an inline
    # bash-style string here.
    wrapper=$(mktemp /tmp/ghostty-wrapper-XXXXXX.sh)
    {
      echo "#!/bin/bash"
      echo "$user_cmd"
    } > "$wrapper"
    chmod 0755 "$wrapper"
    # Background so the caller's polling loop runs concurrently with
    # the surface. The wrapper file leaks one tiny file per invocation —
    # acceptable for a bench refresh; /tmp is wiped on reboot.
    "$GHOSTTY_BIN" -e "$wrapper" &
    disown 2>/dev/null || true
    ;;
  quit)
    echo "ghostty.sh: refusing to quit Ghostty — that would kill the user's work." >&2
    exit 2
    ;;
  *)
    echo "ghostty.sh: unknown sub-command: ${cmd_word:-(none)}" >&2
    echo "  usage: ghostty.sh run-single <cmd>" >&2
    exit 2
    ;;
esac
