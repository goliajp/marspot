#!/usr/bin/env bash
#
# 2026-08-03 report — "I can't close a window's last pane."
#
# Closing panes is the destructive gesture, and it has to go all the
# way down:
#
#   1. Closing the last pane of a window closes the window, and that
#      window does NOT come back on the next launch.
#   2. Closing the last pane of the LAST window quits marspot, and the
#      next launch opens one fresh window — one pane, 1x1 — rather
#      than restoring what was just dismantled.
#
# A script cannot click a pane's [×], so the core does it for itself
# (`MARSPOT_DEV_CLOSE_PANES=n`).
#
# Sandbox-only: MARSPOT_STATE_DIR is redirected and the installed app
# is never touched.  Exits 0 on pass, non-zero on the first failure.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

RUNLOG=/tmp/marspot-test-close-last-pane.log
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

launch() { # extra env assignments passed as VAR=VAL …
  rm -f "$APPLOG"
  > "$RUNLOG"
  env MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" "$@" \
    nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null &
  disown
}

saved_shape() {
  python3 - "$MARSPOT_STATE_DIR" <<'PY'
import struct, sys, pathlib
p = pathlib.Path(sys.argv[1]) / 'shell-state.bin'
if not p.exists():
    print('NONE'); raise SystemExit
b = p.read_bytes()
ver = struct.unpack_from('<I', b, 4)[0]
key, n = struct.unpack_from('<HH', b, 8)
off, shapes = 12, []
for _ in range(n):
    c, r, f, np = struct.unpack_from('<HHHH', b, off); off += 8
    for _ in range(np):
        off += 8
        if ver >= 3: off += 1
        for _ in range(2):
            ln = struct.unpack_from('<H', b, off)[0]; off += 2 + ln
    shapes.append((c, r, np))
print(shapes)
PY
}

# ── phase 1: the last pane of one of two windows ─────────────────────
echo "════ phase 1: emptying one of two windows ════"
cleanup
dev_wipe_state
rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired"
mkdir -p "$MARSPOT_STATE_DIR/logs"

# Two windows.  The restored one becomes key, and the seam closes
# panes in the key window — so that is the one given a single pane.
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

launch MARSPOT_DEV_CLOSE_PANES=1
wait_for WINDOW_RESTORED 40 || fail "the second window never assembled"
sleep 2
wait_for DEV_CLOSE_PANE 30 || fail "the seam never closed a pane"
wait_for WINDOW_EMPTY 20 \
  || fail "closing the last pane did not ask L1 to close the window"
sleep 3

grep -q 'WINDOW_CLOSE_LAST_PANE' "$APPLOG" \
  && fail "with two windows open, emptying one must not quit the app"
pgrep -f "$SHELL_BIN" >/dev/null || fail "the app quit when it should not have"

shape="$(saved_shape)"
echo "==> saved layout after emptying one window: $shape"
[[ "$shape" == "[(2, 1, 2)]" ]] \
  || fail "expected only the surviving 2-pane window saved, got $shape"
echo "==> the emptied window is gone and not remembered"

# ── phase 2: the last pane of the last window ────────────────────────
echo
echo "════ phase 2: emptying the last window ════"
cleanup
sleep 1
launch MARSPOT_DEV_CLOSE_PANES=2
wait_for CORE_LOOP 40 || fail "the core never came up for phase 2"
wait_for WINDOW_CLOSE_LAST_PANE 60 \
  || fail "emptying the last window did not quit marspot"
sleep 3
pgrep -f "$SHELL_BIN" >/dev/null && fail "marspot is still running after the last pane closed"

[[ "$(saved_shape)" == "NONE" ]] || fail "the saved layout outlived the last pane"
[[ -f "$MARSPOT_STATE_DIR/window-state.bin" ]] \
  && fail "the saved geometry outlived the last pane"
echo "==> app quit; both saved files are gone"

# ── phase 3: the next launch is a fresh single window ────────────────
echo
echo "════ phase 3: the launch after that ════"
rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired"
launch
wait_for CORE_LOOP 40 || fail "the fresh launch never came up"
sleep 4

grep -q 'WINDOW_RESTORE_REQUESTED' "$APPLOG" \
  && fail "a fresh launch must not ask for any window to be restored"
painted=$(grep 'WINDOW_FIRST_FRAME' "$APPLOG" | grep -o 'window_id=[0-9]*' \
  | sort -u | grep -c 'window_id=')
(( painted == 1 )) || fail "expected exactly one window, $painted painted"

shape="$(saved_shape)"
echo "==> fresh launch layout: $shape"
[[ "$shape" == "[(1, 1, 1)]" ]] \
  || fail "expected one 1x1 window with one pane, got $shape"
cleanup

echo
echo "PASS — the last pane closes: its window goes with it, the last"
echo "       window takes the app, and the next launch starts clean."
