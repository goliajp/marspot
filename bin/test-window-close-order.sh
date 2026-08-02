#!/usr/bin/env bash
#
# 2026-08-03 report — "whichever window I closed first was the one I
# lost".
#
# Two windows.  Close them one at a time (the last close quits the
# app), relaunch, and the sessions must all still be there — the same
# L3 processes, driving the same PTYs, with the same shells in them.
# Both closing orders have to end up in exactly the same place; that
# they did not was the bug.
#
# What this asserts, per ordering:
#   1. Closing a window leaves its sessions running (no SIGTERM).
#   2. Quitting leaves every session running.
#   3. The relaunch REATTACHES to those exact pids rather than
#      resurrecting new ones, and both windows come back.
#
# The red button cannot be pressed from a script, so the shell presses
# it for itself in a given order (`MARSPOT_DEV_CLOSE_SEQUENCE`).
#
# Sandbox-only: MARSPOT_STATE_DIR is redirected and the installed app
# is never touched.  Exits 0 on pass, non-zero on the first failure.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

RUNLOG=/tmp/marspot-test-close-order.log
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"

fail() {
  echo "FAIL: $*"
  echo "  --- last 40 lines of $APPLOG:"
  tail -40 "$APPLOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

cleanup() { dev_kill_shell_core; }
trap cleanup EXIT

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" || ! -x "$SESSION_BIN" ]]; then
  echo "==> building release binaries"
  ( cd "$ROOT" && cargo build --release \
      --bin marspot-shell --bin marspot-core --bin marspot-session 2>&1 | tail -3 )
fi

wait_for() { # tag, seconds
  local tag="$1" secs="$2" i
  for (( i = 0; i < secs * 10; i++ )); do
    grep -q "$tag" "$APPLOG" 2>/dev/null && return 0
    sleep 0.1
  done
  return 1
}

# Every live session's pid, from the registry entries.
session_pids() {
  python3 - "$MARSPOT_STATE_DIR" <<'PY'
import pathlib, re, sys
root = pathlib.Path(sys.argv[1]) / 'sessions'
out = []
for entry in sorted(root.glob('*/entry.toml')):
    text = entry.read_text()
    sid = re.search(r'^id = (\d+)', text, re.M)
    pid = re.search(r'^pid = (\d+)', text, re.M)
    if sid and pid:
        out.append(f'{sid.group(1)}:{pid.group(1)}')
print(' '.join(out))
PY
}

all_alive() { # "sid:pid sid:pid …"
  local pair pid
  for pair in $1; do
    pid="${pair#*:}"
    kill -0 "$pid" 2>/dev/null || return 1
  done
  return 0
}

run_ordering() { # close-sequence, label
  local sequence="$1" label="$2"

  echo
  echo "════ ordering: close $label ════"
  cleanup
  dev_wipe_state
  rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired"
  mkdir -p "$MARSPOT_STATE_DIR/logs"
  rm -f "$APPLOG"
  > "$RUNLOG"

  # Seed two windows: a 2-pane one and a 1-pane one.  sid 0 = every
  # pane spawns fresh, which is what makes the pids below new.
  python3 - "$MARSPOT_STATE_DIR" <<'PY' || exit 1
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1]); d.mkdir(parents=True, exist_ok=True)
def s(x): b = x.encode(); return struct.pack('<H', len(b)) + b
def pane(sid=0): return struct.pack('<Q', sid) + s('') + s('')
body  = struct.pack('<II', 0xA5505010, 2)
body += struct.pack('<HH', 0, 2)
body += struct.pack('<HHHH', 2, 1, 0, 2) + pane() + pane()
body += struct.pack('<HHHH', 1, 1, 0, 1) + pane()
(d / 'shell-state.bin').write_bytes(body)
frames  = struct.pack('<IIH', 0xA5505011, 2, 2)
frames += struct.pack('<Idddd', 0, 100.0, 400.0, 900.0, 600.0)
frames += struct.pack('<Idddd', 0, 1050.0, 400.0, 700.0, 500.0)
(d / 'window-state.bin').write_bytes(frames)
PY

  echo "==> launching (will close windows in the order: $sequence)"
  MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
    MARSPOT_DEV_CLOSE_SEQUENCE="$sequence" \
    nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null &
  disown

  wait_for WINDOW_RESTORED 40 || fail "[$label] the second window never assembled"
  sleep 3

  local before
  before="$(session_pids)"
  local n_before
  n_before=$(printf '%s' "$before" | wc -w | tr -d ' ')
  echo "==> sessions before the closes: $before"
  (( n_before == 3 )) || fail "[$label] expected 3 sessions, got $n_before"
  all_alive "$before" || fail "[$label] a session was already dead before we started"

  # The sequence's last entry is the last window, so this is a quit.
  wait_for SHELL_QUIT_CLEANUP 60 || fail "[$label] the app never quit"
  # Give the old teardown's SIGTERM + 200 ms grace more than its due.
  sleep 2

  pgrep -f "$SHELL_BIN" >/dev/null && fail "[$label] the shell is still running"

  # 1 + 2 — nothing was killed on the way out.
  local pair pid dead=""
  for pair in $before; do
    pid="${pair#*:}"
    kill -0 "$pid" 2>/dev/null || dead="$dead $pair"
  done
  [[ -z "$dead" ]] \
    || fail "[$label] sessions died with the app:$dead — closing a window must not end its work"
  echo "==> all 3 sessions survived the closes and the quit"

  # 3 — the relaunch reattaches to those exact processes.
  echo "==> relaunching"
  rm -f "$APPLOG"
  MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
    nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null &
  disown
  wait_for WINDOW_RESTORED 40 || fail "[$label] the second window did not come back"
  sleep 3

  local after
  after="$(session_pids)"
  echo "==> sessions after the relaunch: $after"
  [[ "$after" == "$before" ]] \
    || fail "[$label] session pids changed: before [$before] after [$after]"

  local reattached
  reattached=$(grep -c 'L3_REATTACHED' "$APPLOG")
  (( reattached >= 3 )) \
    || fail "[$label] only $reattached session(s) reattached; the rest were respawned"

  local painted
  painted=$(grep 'WINDOW_FIRST_FRAME' "$APPLOG" | grep -o 'window_id=[0-9]*' \
    | sort -u | grep -c 'window_id=')
  (( painted >= 2 )) || fail "[$label] only $painted window(s) came back"

  echo "==> both windows back, all 3 sessions reattached, same pids"
  cleanup
}

# The two orderings that used to disagree.
run_ordering "2,1" "the second window first, then the first"
run_ordering "1,2" "the FIRST window first, then the second"

echo
echo "PASS — closing order does not change what survives:"
echo "       both windows and all three sessions come back either way."
