#!/usr/bin/env bash
#
# A shell that ends on its own ends its pane, and nothing starts it
# again.
#
#   1. Every pane's shell runs a command and exits.  The panes close,
#      their windows close, and the app quits: no core restart, no
#      session resurrected, so the command ran once per pane.  (It used
#      to read as a crash: the core exited, the shell restarted it, and
#      the new core resurrected every session, which ran the command
#      again, every few seconds, for good.)
#   2. One pane's shell exits while the other's lives.  That pane
#      closes and the layout forgets it; after the app is restarted the
#      ended shell is not run again.
#
# Sandbox-only: MARSPOT_STATE_DIR is redirected and the installed app
# is never touched.  Exits 0 on pass, non-zero on the first failure.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

RUNLOG="$(dev_run_log test-all-shells-exit)"
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
STARTS="$MARSPOT_STATE_DIR/starts.log"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"

fail() {
  echo "FAIL: $*"
  echo "  --- shells started:"
  sed 's/^/    /' "$STARTS" 2>/dev/null
  echo "  --- last 40 lines of $APPLOG:"
  tail -40 "$APPLOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

cleanup() { dev_kill_shell_core; }
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

for b in "$SHELL_BIN" "$CORE_BIN" "$SESSION_BIN"; do
  [[ -x $b ]] || { echo "missing $b: build release first"; exit 2; }
done

wait_for() { # tag, seconds
  local tag="$1" secs="$2" i
  for (( i = 0; i < secs * 10; i++ )); do
    grep -q "$tag" "$APPLOG" 2>/dev/null && return 0
    sleep 0.1
  done
  return 1
}

launch() { # the pane shell
  rm -f "$APPLOG"
  > "$RUNLOG"
  env SHELL="$1" MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
    nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
  disown
}

fresh_state() { # python body appending window records to `body`; n windows
  cleanup
  sleep 1
  dev_wipe_state
  rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired" "$MARSPOT_STATE_DIR"/*.d
  rm -f "$STARTS" "$MARSPOT_STATE_DIR/shell-state.bin" "$MARSPOT_STATE_DIR/window-state.bin"
  mkdir -p "$MARSPOT_STATE_DIR/logs"
  python3 - "$MARSPOT_STATE_DIR" "$@" <<'PY' || exit 1
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1])
shapes = [tuple(int(x) for x in a.split('x')) for a in sys.argv[2:]]
def s(x): b = x.encode(); return struct.pack('<H', len(b)) + b
def pane(): return struct.pack('<Q', 0) + s('') + s('')
body  = struct.pack('<II', 0xA5505010, 2)
body += struct.pack('<HH', 0, len(shapes))
for c, r in shapes:
    body += struct.pack('<HHHH', c, r, 0, c * r) + pane() * (c * r)
(d / 'shell-state.bin').write_bytes(body)
frames = struct.pack('<IIH', 0xA5505011, 2, len(shapes))
for i, _ in enumerate(shapes):
    frames += struct.pack('<Idddd', 0, 100.0 + 500 * i, 400.0, 450.0, 400.0)
(d / 'window-state.bin').write_bytes(frames)
PY
}

saved_panes() { # pane count per saved window, or NONE
  python3 - "$MARSPOT_STATE_DIR" <<'PY'
import struct, sys, pathlib
p = pathlib.Path(sys.argv[1]) / 'shell-state.bin'
if not p.exists():
    print('NONE'); raise SystemExit
b = p.read_bytes()
ver = struct.unpack_from('<I', b, 4)[0]
key, n = struct.unpack_from('<HH', b, 8)
off, out = 12, []
for _ in range(n):
    c, r, f, np = struct.unpack_from('<HHHH', b, off); off += 8
    for _ in range(np):
        off += 8
        if ver >= 3: off += 1
        for _ in range(2):
            ln = struct.unpack_from('<H', b, off)[0]; off += 2 + ln
    out.append(np)
print(out)
PY
}

session_dirs() {
  find "$MARSPOT_STATE_DIR/sessions" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l | tr -d ' '
}

starts() { # lines of the start log matching $1
  grep -c "$1" "$STARTS" 2>/dev/null || true
}

pane_shell() { # file, body
  printf '#!/bin/sh\n%s\n' "$2" > "$1"
  chmod +x "$1"
}

# PHASES=2 runs the second phase alone
PHASES="${PHASES:-12}"

phase1() {
# ── phase 1: every shell ends ────────────────────────────────────────
echo "════ phase 1: every pane's shell runs a command and exits ════"
fresh_state 2x1 1x1
ENDS="$MARSPOT_STATE_DIR/ends.sh"
pane_shell "$ENDS" "echo \"start \$\$\" >> '$STARTS'; sleep 3; exit 0"
launch "$ENDS"
wait_for WINDOW_RESTORED 40 || fail "the second window never assembled"
for (( i = 0; i < 400; i++ )); do
  grep -q 'CORE_GONE' "$APPLOG" 2>/dev/null && break
  [[ -z "$(dev_shell_pid)" ]] && break
  sleep 0.1
done
# what would come after a restart: the restart, and the commands again
sleep 10
echo "==> shells started: $(starts start)"
grep -q 'CORE_GONE' "$APPLOG" && fail "the core's exit was taken for a crash and restarted"
grep -q 'session.listener.abnormal_drop' "$APPLOG" \
  && fail "an ended shell's session was kept for resurrection"
grep -q 'WINDOW_CLOSE_LAST_PANE' "$APPLOG" || fail "the app was not asked to quit"
[[ -n "$(dev_shell_pid)" ]] && fail "the app is still running with every shell gone"
(( $(starts start) == 3 )) || fail "3 panes should have run the command 3 times, not $(starts start)"
[[ "$(saved_panes)" == "NONE" ]] || fail "a layout was left to restore: $(saved_panes)"
(( $(session_dirs) == 0 )) || fail "$(session_dirs) session dirs left to resurrect"
echo "==> every pane closed, the app quit, nothing left to resurrect"
}

phase2() {
# ── phase 2: one shell ends, the other lives ─────────────────────────
echo
echo "════ phase 2: one pane's shell exits while another lives ════"
fresh_state 2x1
ONE="$MARSPOT_STATE_DIR/one.sh"
# the first shell to start ends; every later one stays
pane_shell "$ONE" "if mkdir '$MARSPOT_STATE_DIR/first.d' 2>/dev/null; then echo \"end \$\$\" >> '$STARTS'; sleep 3; exit 0; fi
echo \"live \$\$\" >> '$STARTS'; exec sleep 600"
launch "$ONE"
wait_for CORE_LOOP 40 || fail "the core never came up"
for (( i = 0; i < 300; i++ )); do
  [[ "$(saved_panes)" == "[1]" ]] && break
  sleep 0.1
done
sleep 2
[[ "$(saved_panes)" == "[1]" ]] || fail "the ended pane is still in the layout: $(saved_panes)"
[[ -n "$(dev_shell_pid)" ]] || fail "the app quit with a live shell"
grep -q 'CORE_GONE' "$APPLOG" && fail "one shell ending restarted the core"
(( $(session_dirs) == 1 )) || fail "expected the live session's dir only, found $(session_dirs)"
echo "==> the ended pane closed; the other stays"

echo "==> restarting the app"
cleanup
sleep 1
launch "$ONE"
wait_for CORE_LOOP 40 || fail "the core never came up after the restart"
sleep 8
echo "==> shells started: end=$(starts end) live=$(starts live)"
(( $(starts end) == 1 )) || fail "the ended shell ran again"
(( $(starts live) == 2 )) || fail "expected the live pane's shell once per launch, got $(starts live)"
[[ "$(saved_panes)" == "[1]" ]] || fail "the restart brought the ended pane back: $(saved_panes)"
cleanup
}

[[ $PHASES == *1* ]] && phase1
[[ $PHASES == *2* ]] && phase2

echo
echo "PASS — an ended shell closes its pane, the last one quits the app,"
echo "       and none of them is run again."
