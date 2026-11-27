# shellcheck shell=bash
# bin/_multi-window-drag.sh — the pane drag endings of
# bin/test-multi-window.sh, its last phase.  Sourced by it, with its
# fail, wait_for, cleanup, RUNLOG, APPLOG and binaries set.

# --- RFC-006 §8 step 6 — pane drag endings ---------------------------
# A script cannot move the pointer in the sandbox app, so the core's
# `MARSPOT_DEV_DRAG` seam plays the event sequence L1 sends for a real
# drag (press on window 0's first title, past the slop, over the
# target, release) through the production handlers.  Each run seeds
# its own layout, boots, lets the seam drag once, and reads the result
# back from the layout the app itself saved.
#
# `saved` prints one line per saved window: "cols rows live dormant".
saved() {
  python3 - "$MARSPOT_STATE_DIR" <<'PY'
import struct, sys, pathlib
b = (pathlib.Path(sys.argv[1]) / 'shell-state.bin').read_bytes()
magic, ver = struct.unpack_from('<II', b, 0)
assert magic == 0xA5505010 and ver == 3, (hex(magic), ver)
off = 8
_key, n = struct.unpack_from('<HH', b, off); off += 4
for _ in range(n):
    cols, rows, _focus, panes = struct.unpack_from('<HHHH', b, off); off += 8
    live = dormant = 0
    for _ in range(panes):
        off += 8
        flags = b[off]; off += 1
        for _ in range(2):
            (ln,) = struct.unpack_from('<H', b, off); off += 2 + ln
        if flags & 1: dormant += 1
        else: live += 1
    print(cols, rows, live, dormant)
PY
}

# `drag_run <ending> <grids>` — seed (e.g. "2x1,1x1"), boot, drag, settle.
drag_run() {
  local ending="$1" seed="$2"
  cleanup
  rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired"
  rm -f "$APPLOG" "$MARSPOT_STATE_DIR/shell-state.bin" \
        "$MARSPOT_STATE_DIR/window-state.bin"
  : > "$RUNLOG"
  python3 - "$MARSPOT_STATE_DIR" "$seed" <<'PY' || exit 1
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1])
# "CxR" is a grid of live slots; a trailing "d" makes its last slot a
# dormant placeholder (state v3, flags bit 0).
grids = []
for w in sys.argv[2].split(','):
    dormant_last = w.endswith('d')
    c, r = (int(v) for v in w.rstrip('d').split('x'))
    grids.append((c, r, dormant_last))
def pane(flags):
    return struct.pack('<QBHH', 0, flags, 0, 0)
body = struct.pack('<II', 0xA5505010, 3) + struct.pack('<HH', 0, len(grids))
frames = struct.pack('<IIH', 0xA5505011, 2, len(grids))
for i, (c, r, dormant_last) in enumerate(grids):
    n = c * r
    flags = [1 if dormant_last and k == n - 1 else 0 for k in range(n)]
    body += struct.pack('<HHHH', c, r, 0, n) + b''.join(pane(f) for f in flags)
    frames += struct.pack('<Idddd', 0, 100.0 + i * 950.0, 400.0, 900.0, 600.0)
(d / 'shell-state.bin').write_bytes(body)
(d / 'window-state.bin').write_bytes(frames)
PY
  MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
    MARSPOT_DEV_DRAG="$ending" \
    nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
  disown
  wait_for DEV_DRAG_DONE 40 || fail "[$ending $seed] the drag seam never released"
  # The landing reshapes, reattaches and saves off the release; give
  # the new layout time to reach the file.
  sleep 3
  grep 'DEV_DRAG_HOVER' "$APPLOG" | tail -1 | sed 's/^/    /'
}

echo
echo "==> fifth phase: pane drag endings (RFC-006)"

# 1x1 + right band -> 1x2, and the pane that moved out of a 2x1 window
# leaves a dormant placeholder in its slot (the grid does not reshape).
drag_run split-right "2x1,1x1"
got=$(saved)
echo "    split-right from 2x1 onto 1x1 -> $(echo $got | tr '\n' '|')"
[[ "$(sed -n 1p <<<"$got")" == "2 1 1 1" ]] \
  || fail "source window should stay 2x1 with one live pane and one dormant slot, got: $(sed -n 1p <<<"$got")"
[[ "$(sed -n 2p <<<"$got")" == "2 1 2 0" ]] \
  || fail "target 1x1 should become 1x2 (cols 2) with two live panes, got: $(sed -n 2p <<<"$got")"

# The last live pane dragged out of a window closes that window.  A
# lone pane has no title strip to grab, so the source is a live pane
# beside a placeholder — the shape a window has after a move-out.
drag_run split-right "2x1d,1x1"
got=$(saved)
echo "    split-right from 2x1 (one live, one dormant) onto 1x1 -> $(echo $got | tr '\n' '|')"
[[ "$(wc -l <<<"$got" | tr -d ' ')" == "1" ]] \
  || fail "the emptied source window should have closed, saved windows: $(echo $got | tr '\n' '|')"
[[ "$got" == "2 1 2 0" ]] \
  || fail "the surviving window should be 1x2 with both panes, got: $got"

# Released outside every window: a new 1x1 window is born with the
# pane; the source keeps a placeholder where it was.
drag_run new-window "2x1"
# Drag-out is not the Cmd-N path: L1 announces the window
# (WINDOW_OPENED) and the core moves the pane into it (PANE_MOVED).
wait_for PANE_MOVED 30 || fail "[new-window] the pane never moved into a born window"
sleep 3
got=$(saved)
echo "    new-window from 2x1 -> $(echo $got | tr '\n' '|')"
[[ "$(sed -n 1p <<<"$got")" == "2 1 1 1" ]] \
  || fail "source should keep a dormant slot, got: $(sed -n 1p <<<"$got")"
[[ "$(sed -n 2p <<<"$got")" == "1 1 1 0" ]] \
  || fail "a 1x1 window with the dragged pane should exist, got: $(echo $got | tr '\n' '|')"
