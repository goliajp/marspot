#!/usr/bin/env bash
# bin/drivers/marspot.sh — automated marspot launcher.
#
# Usage:
#   bin/drivers/marspot.sh run-app <shell-script-path> [windows]
#     Launch the product (shell, core, one session a pane) in its own
#     state dir with one window (or <windows>) of one pane each, whose
#     shell is the script.
#     `close-app` stops it and removes that state dir.
#
#   bin/drivers/marspot.sh app-pids
#     Shell, core and session pids of that sandbox, one per line.
#
# Why this driver exists: every other terminal needs AppleScript /
# keystroke gymnastics to spawn N parallel workloads; marspot just needs
# its env-var contract.  Having a driver file keeps the scenario
# scripts uniform.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"

cmd=${1:-}
shift || true

# The product runs in a state dir of its own, never the default one.
app_sandbox() {
  export MARSPOT_STATE_DIR="${MARSPOT_BENCH_STATE_DIR:-/tmp/marspot-bench-app}"
  case "$MARSPOT_STATE_DIR" in
    /tmp/marspot-bench-*) ;;
    *) echo "marspot.sh: state dir must be /tmp/marspot-bench-*: $MARSPOT_STATE_DIR" >&2; exit 2 ;;
  esac
  # shellcheck source=/dev/null
  source "$ROOT/bin/_dev-sandbox.sh"
}

case "$cmd" in
  run-app)
    # The product: marspot-shell, which starts marspot-core, which starts
    # one marspot-session for the one pane; <script> is that pane's shell.
    shell_script=${1:-}
    if [[ -z "$shell_script" || ! -x "$shell_script" ]]; then
      echo "marspot.sh: shell script missing or not executable: $shell_script" >&2
      exit 2
    fi
    app_sandbox
    for b in marspot-shell marspot-core marspot-session; do
      if [[ ! -x "$(marspot_bin "$b")" ]]; then
        echo "marspot.sh: $(marspot_bin "$b") is not built" >&2
        exit 2
      fi
    done
    dev_kill_shell_core || true
    rm -rf "$MARSPOT_STATE_DIR"
    mkdir -p "$MARSPOT_STATE_DIR/logs"
    # windows of one pane each, a fixed frame: 99 columns by 35 rows,
    # the most Ghostty fits at its default font on the bench host's
    # 1080-wide display (asked for more, it opens at 99 by 35 anyway).
    # More than one window: each a little up and right of the last.
    windows=${2:-1}
    [[ $windows =~ ^[1-9][0-9]?$ ]] || { echo "marspot.sh: window count: $windows" >&2; exit 2; }
    python3 -c '
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1])
x, y, w, h = (float(v) for v in sys.argv[2].split(","))
n = int(sys.argv[3])
def s(t):
    b = t.encode()
    return struct.pack("<H", len(b)) + b
body = struct.pack("<II", 0xA5505010, 2) + struct.pack("<HH", 0, n)
frames = struct.pack("<IIH", 0xA5505011, 2, n)
for i in range(n):
    body += struct.pack("<HHHH", 1, 1, 0, 1) + struct.pack("<Q", 0) + s("") + s("")
    frames += struct.pack("<Idddd", 0, x + 30 * i, y + 40 * i, w, h)
(d / "shell-state.bin").write_bytes(body)
(d / "window-state.bin").write_bytes(frames)
' "$MARSPOT_STATE_DIR" "${MARSPOT_BENCH_FRAME:-60,400,736,640}" "$windows"
    (cd "$ROOT" && env SHELL="$shell_script" \
      MARSPOT_SESSION_BIN="$(marspot_bin marspot-session)" \
      MARSPOT_CORE_BIN="$(marspot_bin marspot-core)" \
      nohup "$(marspot_bin marspot-shell)" > /dev/null 2>&1 < /dev/null & dev_record_pid $!) || true
    disown 2>/dev/null || true
    ;;
  close-app)
    app_sandbox
    dev_kill_shell_core || true
    rm -rf "$MARSPOT_STATE_DIR"
    ;;
  app-pids)
    # shell, core and session of the running sandbox, one per line
    app_sandbox
    dev_shell_pid
    dev_core_pids
    dev_session_pids
    ;;
  app-version)
    for b in marspot-shell marspot-core marspot-session; do
      MARSPOT_NO_REDIRECT=1 "$(marspot_bin "$b")" --version 2>/dev/null | head -1
    done
    ;;
  *)
    echo "marspot.sh: unknown sub-command: ${cmd:-(none)}" >&2
    echo "  usage: marspot.sh run-app <script> [windows]  # the product, one pane a window" >&2
    echo "         marspot.sh close-app                   # stop it, remove its state" >&2
    echo "         marspot.sh app-pids                    # its shell, core and session pids" >&2
    exit 2
    ;;
esac
