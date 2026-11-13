#!/usr/bin/env bash
# bin/drivers/marspot.sh — automated marspot launcher.
#
# Usage:
#   bin/drivers/marspot.sh run-app <shell-script-path>
#     Launch the product (shell, core, one session) in its own state
#     dir with one window and one pane whose shell is the script.
#     `close-app` stops it and removes that state dir.
#
#   bin/drivers/marspot.sh run-shell <shell-script-path> [extra-args...]
#     Launch marspot with MARSPOT_SHELL pointed at <shell-script-path>.  The
#     script becomes the shell for every spawned session — so when marspot
#     boots into its 3×3 multi-session layout, all 9 sessions execute
#     <shell-script-path>.
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

# Common: build the requested binary if missing.
ensure_build() {
  local bin=$1
  if [[ ! -x "$(marspot_bin "$bin")" ]]; then
    ( cd "$ROOT" && cargo build --release --bin "$bin" 2>&1 | tail -3 ) >&2
  fi
}

case "$cmd" in
  run-shell|run-shell-mcli)
    # run-shell      → launch marspot (multi-session, 3×3)
    # run-shell-mcli → launch mcli (single-session) — used by scenarios
    #                  that need exactly one session, since marspot auto-
    #                  spawns 9 and applies MARSPOT_SHELL to every one.
    bin=marspot
    [[ "$cmd" == "run-shell-mcli" ]] && bin=mcli
    shell_script=${1:-}
    shift || true
    if [[ -z "$shell_script" || ! -x "$shell_script" ]]; then
      echo "marspot.sh: shell script missing or not executable: $shell_script" >&2
      exit 2
    fi
    ensure_build "$bin"
    kill_app "$bin" || true
    # Forward profiling env-vars so per-trial counters can be captured.
    env_pass=""
    for v in MARSPOT_PROFILE MARSPOT_LATENCY MARSPOT_SCALE MARSPOT_TMUX_DEBUG; do
      if [[ -n "${!v:-}" ]]; then env_pass+=" $v=${!v}"; fi
    done
    # nohup + redirect so the bench harness isn't tied to marspot's output;
    # its result is reported through the marker file.
    bin_path="$(marspot_bin "$bin")"
    # The pid is written down so kill_app stops this instance and only
    # this one (env and nohup exec, so $! is the binary's pid).
    (cd "$ROOT" && env $env_pass MARSPOT_SHELL="$shell_script" \
      nohup "$bin_path" "$@" > /dev/null 2>&1 < /dev/null & bench_record_pid $!) || true
    disown 2>/dev/null || true
    # Don't `wait` — marspot exits when all sessions exit, scenario script polls marker.
    ;;
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
    # one window, one pane, a fixed frame: 99 columns by 35 rows, the
    # most Ghostty fits at its default font on the bench host's
    # 1080-wide display (asked for more, it opens at 99 by 35 anyway)
    python3 -c '
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1])
x, y, w, h = (float(v) for v in sys.argv[2].split(","))
def s(t):
    b = t.encode()
    return struct.pack("<H", len(b)) + b
body = struct.pack("<II", 0xA5505010, 2) + struct.pack("<HH", 0, 1)
body += struct.pack("<HHHH", 1, 1, 0, 1) + struct.pack("<Q", 0) + s("") + s("")
(d / "shell-state.bin").write_bytes(body)
frames = struct.pack("<IIH", 0xA5505011, 2, 1) + struct.pack("<Idddd", 0, x, y, w, h)
(d / "window-state.bin").write_bytes(frames)
' "$MARSPOT_STATE_DIR" "${MARSPOT_BENCH_FRAME:-60,400,736,640}"
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
    echo "  usage: marspot.sh run-app        <script>  # the product, one pane" >&2
    echo "         marspot.sh close-app                # stop it, remove its state" >&2
    echo "         marspot.sh run-shell      <script>  # multi-session (3×3 grid)" >&2
    echo "         marspot.sh run-shell-mcli <script>  # single-session (mcli)" >&2
    exit 2
    ;;
esac
