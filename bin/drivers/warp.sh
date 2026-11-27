#!/usr/bin/env bash
# bin/drivers/warp.sh — Warp launcher.
#
# Warp has no real AppleScript API.  We can launch the app via `open`
# and either (a) use System-Events keystroke to type a command (very
# fragile — focus races, Warp's command-bar interception, autocompletion
# rewriting), or (b) ask the user to paste a known one-liner once.
#
# This driver does (a) for run-single (one window, one command) and
# falls back to **paste mode** for run-windows (N parallel commands) —
# scripting N keystroked commands into separate windows is far below
# the reliability bar we want for a recurring bench.
#
# Usage:
#   bin/drivers/warp.sh run-single <cmd>
#     Open a fresh Warp window, type <cmd>, hit return.
#
#   bin/drivers/warp.sh run-launch <cmd>
#     Open a Warp window that runs <cmd>, through a launch
#     configuration and the warp:// URL scheme.
#
#   bin/drivers/warp.sh <run-sub-command> --pid-file <file> ...
#     Any run sub-command: also append the app's pid to <file>.
#
#   bin/drivers/warp.sh paste-block <n> <cmd>
#     Print a paste-ready block: open Warp, hit cmd-N <n-1> times,
#     paste the block in each window.  Caller polls the marker.
#
#   bin/drivers/warp.sh quit

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"
# shellcheck source=bin/_app-pids.sh
source "$ROOT/bin/_app-pids.sh"

cmd_word=${1:-}
shift || true
# --pid-file <file>: append the app's pid there when this launch is what
# started it (bin/_app-pids.sh), so the caller can stop exactly that.
pid_file=""
if [[ ${1:-} == --pid-file ]]; then
  pid_file=${2:?pid-file}
  shift 2
fi
# what of the app runs just before the launch, and when the launch began
pre_launch() {
  [[ -z $pid_file ]] && return 0
  pid_before=$(app_pid "$1")
  launched_at=$(date +%s)
}

case "$cmd_word" in
  run-single)
    user_cmd=${1:?cmd}; shift || true
    # Same paste-threshold pattern as iterm.sh / terminal.sh — Warp's
    # AI command-bar intercepts and rewrites long typed input even when
    # SE keystroke succeeds at the OS level. Stash the long cmd in a
    # wrapper file, keystroke only `bash <wrapper>; exit` so Warp's
    # interceptor sees a tiny invocation it can't mangle.
    wrapper=$(mktemp /tmp/warp-wrapper-XXXXXX)
    printf '#!/bin/bash\n%s\n' "$user_cmd" > "$wrapper"
    chmod 0755 "$wrapper"
    short_cmd="bash $wrapper; exit"
    pre_launch dev.warp.Warp-Stable
    open -a Warp
    [[ -z $pid_file ]] || record_app_pid dev.warp.Warp-Stable "$pid_file" "$pid_before" "$launched_at"
    sleep 0.8  # focus settle
    osascript <<APPLESCRIPT >/dev/null
tell application "System Events"
  tell process "stable"
    set frontmost to true
  end tell
  delay 0.2
  keystroke "$short_cmd"
  keystroke return
end tell
APPLESCRIPT
    ;;
  run-launch)
    # No keystrokes: a launch configuration names the command, and the
    # app's own URL scheme opens it.  Reaches Warp from where System
    # Events is not allowed to type (an ssh session).
    user_cmd=${1:?cmd}; shift || true
    wrapper=$(mktemp /tmp/warp-wrapper-XXXXXX)
    printf '#!/bin/bash\n%s\n' "$user_cmd" > "$wrapper"
    chmod 0755 "$wrapper"
    conf_dir="$HOME/.warp/launch_configurations"
    mkdir -p "$conf_dir"
    cat > "$conf_dir/marspot-bench.yaml" <<YAML
---
name: marspot-bench
windows:
  - tabs:
      - layout:
          cwd: /tmp
          commands:
            - exec: $wrapper; exit
YAML
    pre_launch dev.warp.Warp-Stable
    open "warp://launch/marspot-bench.yaml"
    [[ -z $pid_file ]] || record_app_pid dev.warp.Warp-Stable "$pid_file" "$pid_before" "$launched_at"
    ;;
  paste-block)
    n=${1:?n}; shift
    user_cmd=${1:?cmd}; shift || true
    cat <<EOF
----- Warp manual paste (n=$n) -----
1. Open Warp (cmd+space → Warp)
2. Hit cmd+N $((n-1)) times to spawn $n windows (or skip for n=1)
3. Click on each window, paste:
$user_cmd
4. Hit return
-------------------------------------
EOF
    ;;
  quit)
    echo "warp.sh: refusing to quit Warp — that would kill the user's work." >&2
    exit 2
    ;;
  *)
    echo "warp.sh: unknown sub-command: ${cmd_word:-(none)}" >&2
    exit 2
    ;;
esac
