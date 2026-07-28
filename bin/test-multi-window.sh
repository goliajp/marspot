#!/usr/bin/env bash
#
# RFC-005 — end-to-end check that marspot really runs more than one
# window.
#
# Cmd-N cannot be driven from a script (AppleScript cannot focus the
# sandbox app to deliver a keystroke), but the restore path exercises
# strictly more of the same machinery: L2 asks → L1 opens the window
# and gives it a surface pair → L2 adopts it → the off-loop assembly
# fills its panes.  The only thing Cmd-N adds on top is the keystroke.
#
# What this asserts, all from the structured log + the state file:
#   1. A two-window `shell-state.bin` really brings up two windows.
#   2. Each window gets its OWN surface pair and its own frames — the
#      bug that made window 1 freeze when window 2 attached.
#   3. The second window's panes land in the second window, and the
#      boot window does not swallow them as orphans.
#   4. Both windows are still in the file after the run (the 2026-07-26
#      incident: opening a window overwrote the layout with one pane).
#
# Sandbox-only: MARSPOT_STATE_DIR is redirected and the installed app
# is never touched.  Exits 0 on pass, non-zero on the first failure.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

RUNLOG=/tmp/marspot-test-multi-window.log
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"

fail() {
  echo "FAIL: $*"
  echo "  --- last 40 lines of $APPLOG:"
  tail -40 "$APPLOG" 2>/dev/null | sed 's/^/    /'
  echo "  --- last 20 lines of $RUNLOG:"
  tail -20 "$RUNLOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

cleanup() { dev_kill_shell_core; }
trap cleanup EXIT

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" || ! -x "$SESSION_BIN" ]]; then
  echo "==> building release binaries"
  ( cd "$ROOT" && cargo build --release \
      --bin marspot-shell --bin marspot-core --bin marspot-session 2>&1 | tail -3 )
fi

cleanup
dev_wipe_state
# Deterministic start: no leftover sessions to reattach or adopt.
rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired"
mkdir -p "$MARSPOT_STATE_DIR/logs"
rm -f "$APPLOG"
> "$RUNLOG"

# --- seed a two-window layout ---------------------------------------
# Written here rather than by the app so the test starts from a known
# shape.  sid 0 = "no surviving session", i.e. every pane spawns fresh.
echo "==> seeding a two-window state into $MARSPOT_STATE_DIR"
python3 - "$MARSPOT_STATE_DIR" <<'PY' || exit 1
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1])
d.mkdir(parents=True, exist_ok=True)

def s(x: str) -> bytes:
    b = x.encode()
    return struct.pack('<H', len(b)) + b

def pane(sid=0, title='', cwd=''):
    return struct.pack('<Q', sid) + s(title) + s(cwd)

# shell-state.bin v2: window 0 = 2x1 with two panes, window 1 = 1x1.
body  = struct.pack('<II', 0xA5505010, 2)
body += struct.pack('<HH', 0, 2)          # key_window, window_count
body += struct.pack('<HHHH', 2, 1, 0, 2) + pane() + pane(title='second')
body += struct.pack('<HHHH', 1, 1, 0, 1) + pane(title='other window')
(d / 'shell-state.bin').write_bytes(body)

# window-state.bin v2: one frame per window, side by side.
frames  = struct.pack('<IIH', 0xA5505011, 2, 2)
frames += struct.pack('<Idddd', 0, 100.0, 400.0, 900.0, 600.0)
frames += struct.pack('<Idddd', 0, 1050.0, 400.0, 700.0, 500.0)
(d / 'window-state.bin').write_bytes(frames)
print('seeded')
PY

# --- boot ------------------------------------------------------------
# Debug level on the core: the per-frame `render.frame` line carries
# window_id + surface_id, which is how this test proves the two windows
# are painting into different pairs rather than sharing one.
echo "==> launching marspot-shell"
MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  MARSPOT_LOG_CORE=debug \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null &
disown

wait_for() { # tag, seconds
  local tag="$1" secs="$2" i
  for (( i = 0; i < secs * 10; i++ )); do
    grep -q "$tag" "$APPLOG" 2>/dev/null && return 0
    sleep 0.1
  done
  return 1
}

wait_for WINDOW_RESTORE_REQUESTED 20 \
  || fail "core never asked for the saved windows (state file not read as v2?)"
wait_for WINDOW_RESTORE 20 || fail "L1 never acted on the restore request"
wait_for WINDOW_RESTORING 20 || fail "core never adopted the restored window"
wait_for WINDOW_RESTORED 30 || fail "the off-loop assembly never landed its panes"

# Let both windows settle into steady-state rendering.
sleep 3

# --- 1 + 2. two windows, each painting its own pair -------------------
adopted=$(grep -c 'WINDOW_RESTORING\|WINDOW_ADOPTED' "$APPLOG")
(( adopted >= 1 )) || fail "expected a second window to be adopted"

opened=$(grep -o 'WINDOW_OPENED' "$APPLOG" | wc -l | tr -d ' ')
(( opened >= 1 )) || fail "L1 never reported opening the second window"

# Both windows actually paint, each into its OWN pair.  Before step 4d
# the second window's attach overwrote the single global pair and the
# first window froze on its last frame — WINDOW_FIRST_FRAME is emitted
# once per window (unsampled, unlike the per-frame debug line) and
# carries the surface it painted into, so this pins that regression.
painted=$(grep 'WINDOW_FIRST_FRAME' "$APPLOG" | grep -o 'window_id=[0-9]*' | sort -u)
n_painted=$(printf '%s' "$painted" | grep -c 'window_id=')
ids=$(grep 'WINDOW_FIRST_FRAME' "$APPLOG" | grep -o 'surface_id=[0-9]*' | sort -u \
  | wc -l | tr -d ' ')
echo "==> windows that painted: $(echo $painted | tr '\n' ' ') (distinct surfaces: $ids)"
(( n_painted >= 2 )) \
  || fail "only $n_painted window(s) ever painted a frame — the other is frozen"
(( ids >= 2 )) \
  || fail "both windows painted into the same surface — they share a pair"

# --- 2b. the present side, not just the paint side --------------------
# 2026-07-28: a window can pass every assertion above and still be
# BLACK — core painted, shell installed the pair, but the window had no
# presenter (the layer that puts the IOSurface on the NSView), and the
# skip was silent.  Every non-boot window must log PRESENTER_READY, and
# the no-presenter error must never fire.
presenters=$(grep -c 'WINDOW_PRESENTER_READY' "$APPLOG")
(( presenters >= 1 )) \
  || fail "the restored window has no presenter — it is a black rectangle"
grep -q 'shell.surface_ready.no_presenter' "$APPLOG" \
  && fail "a pair was acked into a presenter-less window"
# …and the presenter must have actually PRESENTED.  "painted + pair
# installed + presenter exists" all logged green while the window was
# still black — present() was only ever driven for the boot window.
presented=$(grep 'WINDOW_FIRST_PRESENT' "$APPLOG" | grep -o 'window_id=[0-9]*' | sort -u)
n_presented=$(printf '%s' "$presented" | grep -c 'window_id=')
echo "==> windows that PRESENTED: $(echo $presented | tr '\n' ' ')"
(( n_presented >= 2 )) \
  || fail "only $n_presented window(s) ever presented — the other shows black"

# --- 3. no cross-window orphan adoption -------------------------------
if grep -q 'core.boot.orphan_adopted' "$APPLOG"; then
  fail "boot window adopted sessions it should have left to the other window"
fi

# --- 4. the layout survives -------------------------------------------
python3 - "$MARSPOT_STATE_DIR" <<'PY' || fail "saved state is no longer two windows"
import struct, sys, pathlib
b = (pathlib.Path(sys.argv[1]) / 'shell-state.bin').read_bytes()
magic, ver = struct.unpack_from('<II', b, 0)
assert magic == 0xA5505010, hex(magic)
assert ver == 2, ver
key, n = struct.unpack_from('<HH', b, 8)
print(f'saved: {n} window(s), key_window={key}')
assert n == 2, f'expected 2 windows in the saved file, got {n}'
PY

frames=$(python3 - "$MARSPOT_STATE_DIR" <<'PY'
import struct, sys, pathlib
b = (pathlib.Path(sys.argv[1]) / 'window-state.bin').read_bytes()
print(struct.unpack_from('<H', b, 8)[0])
PY
)
[[ "$frames" == "2" ]] || fail "window-state.bin holds $frames frame(s), expected 2"

# --- 5. the Cmd-N path: a brand-new window, not a restored one --------
# Same code Cmd-N runs (`open_new_window`), reached through a dev seam
# because a keystroke cannot be delivered to the sandbox app.  What is
# specific to it: a 1x1 grid with one freshly-spawned session, none of
# which the restore path exercises.
echo
echo "==> second phase: fresh window via the Cmd-N path"
cleanup
rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired"
rm -f "$APPLOG" "$MARSPOT_STATE_DIR/shell-state.bin" \
      "$MARSPOT_STATE_DIR/window-state.bin"
> "$RUNLOG"

MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  MARSPOT_LOG_CORE=debug MARSPOT_DEV_EXTRA_WINDOWS=1 \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null &
disown

wait_for WINDOW_ADOPTED 30 || fail "the fresh window was never adopted by the core"
sleep 3

grep -q 'WINDOW_RESTORING' "$APPLOG" \
  && fail "a fresh window must not consume a restore record"

painted=$(grep 'WINDOW_FIRST_FRAME' "$APPLOG" | grep -o 'window_id=[0-9]*' | sort -u)
n_painted=$(printf '%s' "$painted" | grep -c 'window_id=')
echo "==> windows that painted: $(echo $painted | tr '\n' ' ')"
(( n_painted >= 2 )) || fail "the fresh window never painted ($n_painted painted)"
grep -q 'WINDOW_PRESENTER_READY' "$APPLOG" \
  || fail "the Cmd-N window has no presenter — it is a black rectangle"
grep -q 'shell.surface_ready.no_presenter' "$APPLOG" \
  && fail "a pair was acked into a presenter-less window (Cmd-N phase)"
n_presented=$(grep 'WINDOW_FIRST_PRESENT' "$APPLOG" | grep -o 'window_id=[0-9]*' | sort -u | grep -c 'window_id=')
(( n_presented >= 2 )) \
  || fail "the Cmd-N window never presented — it shows black"

# The new window comes up 1x1 with one pane, and — the 2026-07-26
# incident — must not shrink what the boot window persisted.
python3 - "$MARSPOT_STATE_DIR" <<'PY' || fail "the fresh window damaged the saved layout"
import struct, sys, pathlib
b = (pathlib.Path(sys.argv[1]) / 'shell-state.bin').read_bytes()
key, n = struct.unpack_from('<HH', b, 8)
off = 12
shapes = []
for _ in range(n):
    c, r, f, np = struct.unpack_from('<HHHH', b, off); off += 8
    for _ in range(np):
        off += 8
        for _ in range(2):
            ln = struct.unpack_from('<H', b, off)[0]; off += 2 + ln
    shapes.append((c, r, np))
print(f'saved: {n} window(s) {shapes}, key_window={key}')
assert n == 2, f'expected both windows saved, got {n}'
assert shapes[0][2] >= 2, f'boot window lost panes: {shapes[0]}'
assert shapes[1] == (1, 1, 1), f'fresh window should be 1x1 with one pane: {shapes[1]}'
PY

# --- 5b. closing that window must not take the app with it ------------
# 2026-07-28: closing a second window killed the whole app with SIGSEGV
# inside `-[_NSWindowTransformAnimation dealloc]` — the window was
# over-released (AppKit's `releasedWhenClosed` plus our own `Retained`)
# and freed while the ordering animation still held it.
#
# A script cannot press the red button, so the shell presses it for
# itself (`MARSPOT_DEV_CLOSE_EXTRA` → `performClose:`).  It also
# activates the app and makes that window key first, and both are
# load-bearing: closing a NON-key window in a background app does not
# crash even without the fix — the animation that trips over the freed
# window is the one AppKit runs when it hands key status on.  The
# focus theft lasts a fraction of a second.
echo
echo "==> closing the extra window (real close button; steals focus briefly)"
cleanup
rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired"
rm -f "$APPLOG" "$MARSPOT_STATE_DIR/shell-state.bin" \
      "$MARSPOT_STATE_DIR/window-state.bin"
> "$RUNLOG"

MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  MARSPOT_DEV_EXTRA_WINDOWS=1 MARSPOT_DEV_CLOSE_EXTRA=1 \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null &
disown
SHELL_PID=$!

wait_for DEV_CLOSE_EXTRA 30 || fail "the extra window never reached the close path"
wait_for "closed one window" 15 || fail "the close never completed"
sleep 3

kill -0 "$SHELL_PID" 2>/dev/null \
  || fail "the shell died closing a window (check DiagnosticReports for a .ips)"
grep -q 'PANIC' "$APPLOG" && fail "a panic was logged while closing the window"
# And it must still be a working app afterwards, not a live husk.
opened_after=$(grep -c 'WINDOW_CLOSED' "$APPLOG")
(( opened_after >= 1 )) || fail "no WINDOW_CLOSED recorded"
echo "==> shell survived the close (pid $SHELL_PID still up)"

# --- 6. a core swap with two windows open -----------------------------
# The riskiest thing about multi-window: a replacement core learns
# about the boot window from its env pair and about the others from the
# attach each sent when it opened — which a replacement missed.  L1 now
# replays them (`announce_windows_to_new_core`), and refuses the
# replacement core's restore requests (`core_generation`) so the
# already-open windows are not opened a second time.
#
# SIGKILL is the cheapest way to get a fresh core against a live L1;
# the silent-update path spawns one exactly the same way.
echo
echo "==> third phase: core swap with two windows open"
cleanup
rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired"
rm -f "$APPLOG" "$MARSPOT_STATE_DIR/shell-state.bin" \
      "$MARSPOT_STATE_DIR/window-state.bin"
> "$RUNLOG"
MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  MARSPOT_LOG_CORE=debug MARSPOT_DEV_EXTRA_WINDOWS=1 \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null &
disown
wait_for WINDOW_ADOPTED 30 || fail "the second window never came up for the swap phase"
sleep 2
mark=$(wc -l < "$APPLOG")
CORE_PID=$(pgrep -f "$MARSPOT_STATE_DIR/binaries/.*/marspot-core( |$)" | head -1)
[[ -z "$CORE_PID" ]] && CORE_PID=$(pgrep -f "$CORE_BIN( |$)" | head -1)
[[ -n "$CORE_PID" ]] || fail "could not find the running core to kill"
echo "==> SIGKILL core pid $CORE_PID"
kill -9 "$CORE_PID"

for _ in $(seq 1 300); do
  tail -n "+$((mark + 1))" "$APPLOG" | grep -q 'WINDOW_REANNOUNCED' && break
  sleep 0.1
done
after() { tail -n "+$((mark + 1))" "$APPLOG"; }

after | grep -q 'WINDOW_REANNOUNCED' \
  || fail "the replacement core was never told about the second window"

sleep 3
repainted=$(after | grep 'WINDOW_FIRST_FRAME' | grep -o 'window_id=[0-9]*' | sort -u)
n_repainted=$(printf '%s' "$repainted" | grep -c 'window_id=')
echo "==> windows painting under the new core: $(echo $repainted | tr '\n' ' ')"
(( n_repainted >= 2 )) \
  || fail "only $n_repainted window(s) painted after the swap — the rest are frozen"

# The replacement core re-reads the same saved file; honouring its
# restore requests would stack a second copy of every window.
if after | grep -q 'WINDOW_RESTORE\b'; then
  fail "L1 honoured a restore request from a replacement core"
fi
after | grep -q 'WINDOW_RESTORE_IGNORED' \
  || echo "    (note: replacement core did not ask to restore — fine)"

if after | grep -q 'core.boot.orphan_adopted'; then
  fail "the new core's boot window adopted the other window's live panes"
fi

python3 - "$MARSPOT_STATE_DIR" <<'PY' || fail "the swap damaged the saved layout"
import struct, sys, pathlib
b = (pathlib.Path(sys.argv[1]) / 'shell-state.bin').read_bytes()
key, n = struct.unpack_from('<HH', b, 8)
print(f'saved after swap: {n} window(s), key_window={key}')
assert n == 2, f'expected 2 windows after the swap, got {n}'
PY

# --- 7. dual-window silent update (UPDATE_SWAP) -----------------------
# RFC-005 step 7 — the same guarantee as phase 6 (SIGKILL), through
# the REAL update machinery: stage a pending core, SIGUSR1-trigger the
# swap, and both windows must come back painting under the new core
# with no duplicated windows.  First proven in production on
# 2026-07-28 (the 0.12.56 install ran with two windows open); this
# pins it.
echo
echo "==> fourth phase: silent update with two windows open"
mark=$(wc -l < "$APPLOG")
mkdir -p "$MARSPOT_STATE_DIR/binaries/pending"
cp "$CORE_BIN" "$MARSPOT_STATE_DIR/binaries/pending/marspot-core"
"$SHELL_BIN" --trigger >/dev/null 2>&1 || fail "--trigger did not reach the shell"

after7() { tail -n "+$((mark + 1))" "$APPLOG"; }
for i in $(seq 1 300); do
  after7 | grep -q 'UPDATE_SWAP' && break
  sleep 0.1
done
after7 | grep -q 'UPDATE_SWAP' || fail "the staged core never swapped in"
sleep 3

after7 | grep -q 'WINDOW_REANNOUNCED' \
  || fail "the swapped-in core was never told about the second window"
repainted=$(after7 | grep 'WINDOW_FIRST_FRAME' | grep -o 'window_id=[0-9]*' | sort -u)
n_repainted=$(printf '%s' "$repainted" | grep -c 'window_id=')
echo "==> windows painting under the swapped core: $(echo $repainted | tr '\n' ' ')"
(( n_repainted >= 2 )) \
  || fail "only $n_repainted window(s) painted after UPDATE_SWAP"
if after7 | grep -q 'WINDOW_RESTORE\b'; then
  fail "the swapped-in core re-restored windows that were already open"
fi
after7 | grep -q 'core.boot.orphan_adopted' \
  && fail "the swapped-in core adopted the other window's panes as orphans"

echo
echo "PASS — restore path: two windows, own surface pairs, layout survived;"
echo "       Cmd-N path: fresh 1x1 window painted without narrowing the file;"
echo "       core swap:  both windows re-announced and painting, no duplicates;"
echo "       update:     UPDATE_SWAP with two windows — both back, none doubled."
