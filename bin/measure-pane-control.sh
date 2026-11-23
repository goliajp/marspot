#!/usr/bin/env bash
# bin/measure-pane-control.sh — the pane control socket's latencies, in a
# sandboxed app of one window with two panes: one pane reads its input
# and prints nothing, the other sits at a /bin/sh prompt.  What each
# number is: bin/_pane_control_probe.py.
#
#   PANECTL_ROUNDS   samples per latency (default 100; halved for the
#                    slower ones)
#   PANECTL_OUT      JSON written here (default build/pane-control.json)
#
# Prints `pane-control <name> p95 <ms>`.  Opens a window and is a
# measurement, so it runs on the runner, in the console session:
#   E2E_LOCK=bench REMOTE_NAME=<own name> bin/test-e2e-remote.sh measure-pane-control.sh
# bin/bench.sh --full runs it and bin/_bench_gate.py judges it.
set -uo pipefail
trap 'exit 1' HUP INT TERM

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

OUT="${PANECTL_OUT:-$ROOT/build/pane-control.json}"
BIN="$ROOT/target/release"
SD="$MARSPOT_STATE_DIR"
APPLOG="$SD/logs/marspot.log"
PEER="$BIN/marspot-peer"

fail() {
  echo "FAIL: $*"
  tail -20 "$APPLOG" 2>/dev/null | sed 's/^/    /'
  exit 2
}
trap dev_kill_shell_core EXIT
for b in marspot-shell marspot-core marspot-session marspot-peer; do
  [[ -x $BIN/$b ]] || fail "$BIN/$b is not built"
done
mkdir -p "$(dirname "$OUT")"

dev_kill_shell_core
dev_wipe_state
rm -rf "$SD/sessions" "$SD/retired"
mkdir -p "$SD/logs"
PANE_SH="$SD/pane.sh"
printf '#!/bin/sh\nPS1="$ " exec /bin/sh\n' > "$PANE_SH"
chmod +x "$PANE_SH"
python3 - "$SD" <<'PY' || fail "could not write the layout"
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1])
def s(x): b = x.encode(); return struct.pack('<H', len(b)) + b
def pane(): return struct.pack('<Q', 0) + s('') + s('')
body = struct.pack('<II', 0xA5505010, 2) + struct.pack('<HH', 0, 1)
body += struct.pack('<HHHH', 2, 1, 0, 2) + pane() + pane()
(d / 'shell-state.bin').write_bytes(body)
frames = struct.pack('<IIH', 0xA5505011, 2, 1) + struct.pack('<Idddd', 0, 60.0, 300.0, 1200.0, 640.0)
(d / 'window-state.bin').write_bytes(frames)
PY

env SHELL="$PANE_SH" MARSPOT_SESSION_BIN="$BIN/marspot-session" \
  nohup "$BIN/marspot-shell" > "$(dev_run_log pane-control)" 2>&1 < /dev/null &
dev_record_pid $!
disown

for (( i = 0; i < 450; i++ )); do
  grep -qa $'\tPANE_CONTROL_CAPS_SEEN\t' "$APPLOG" 2>/dev/null && break
  sleep 0.2
done
CAPS=$(grep -a $'\tPANE_CONTROL_CAPS_SEEN\t' "$APPLOG" | tail -1 | grep -o $'\tcaps=[0-9]*' | cut -d= -f2)
[[ -n $CAPS ]] || fail "the core never said what it does with pane control"
PANES=""
for (( i = 0; i < 100; i++ )); do
  PANES=$("$PEER" pane list --json 2>/dev/null | python3 -c '
import json,sys
ps = json.load(sys.stdin)["result"]["panes"]
print(" ".join(str(p["id"]) for p in sorted((p for p in ps if p["cell"]), key=lambda p: p["cell"])))' 2>/dev/null)
  (( $(wc -w <<<"$PANES") == 2 )) && break
  sleep 0.5
done
read -r RAW SH <<<"$PANES"
[[ -n ${SH:-} ]] || fail "two panes expected, got '$PANES'"
for p in $RAW $SH; do
  for (( i = 0; i < 40; i++ )); do
    "$PEER" pane run "$p" --timeout 5 true >/dev/null 2>&1 && break
    sleep 0.5
  done
done
"$PEER" pane send "$RAW" 'stty raw -echo; exec cat > /dev/null' >/dev/null || fail "could not start the reader in pane $RAW"
sleep 1

echo "==> caps=$CAPS, panes $RAW (reader) and $SH (sh), load1 $(sysctl -n vm.loadavg | awk '{print $2}')"
PANECTL_CAPS="$CAPS" python3 "$ROOT/bin/_pane_control_probe.py" "$SD/control.sock" "$RAW" "$SH" "$OUT" || fail "the probe stopped"
