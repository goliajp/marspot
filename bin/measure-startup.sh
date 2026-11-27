#!/usr/bin/env bash
# bin/measure-startup.sh — how long the shipped three processes take
# from launching marspot-shell to the first frame on screen that shows
# the pane's own output, with one window of one pane.
#
# The clock starts right before marspot-shell is exec'd and stops at
# the core's WINDOW_FIRST_CONTENT: the shell has reported the first
# frame in which the pane shows what its shell printed as shown.
#
#   cold   the first launch of binaries copied and ad-hoc signed again a
#          moment ago: new inodes and a code hash the machine has never
#          seen, so macOS assesses each on its first exec and keeps no
#          compiled shaders for the core -- what the first start after
#          an install pays.  A plain copy was not cold: on mini it read
#          the same as a relaunch.  (Purging the file cache needs root;
#          the binaries' pages are in it from the copy.)
#   warm   the same copies launched again right after
#
#   STARTUP_TRIALS   cold + warm pairs (default 5); the median of each
#   STARTUP_OUT      JSON written here (default build/startup.json)
#
# Prints `startup cold <ms>` and `startup warm <ms>`.  Opens a window
# and is a measurement, so it runs on the runner, in the console
# session, alone:
#   E2E_LOCK=bench REMOTE_NAME=<own name> bin/test-e2e-remote.sh measure-startup.sh
set -uo pipefail
trap 'exit 1' HUP INT TERM

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
dev_no_dock

TRIALS="${STARTUP_TRIALS:-5}"
OUT="${STARTUP_OUT:-$ROOT/build/startup.json}"
BIN="$ROOT/target/release"
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
GAUGE="$ROOT/bin/_app_gauge.py"
COPIES="$MARSPOT_STATE_DIR/copies"

fail() {
  echo "FAIL: $*"
  tail -20 "$APPLOG" 2>/dev/null | sed 's/^/    /'
  exit 2
}
trap 'dev_kill_shell_core; rm -rf "${COPIES:?}"' EXIT
for b in marspot-shell marspot-core marspot-session; do
  [[ -x $BIN/$b ]] || fail "$BIN/$b is not built"
done
mkdir -p "$MARSPOT_STATE_DIR" "$(dirname "$OUT")"

PANE_SH="$MARSPOT_STATE_DIR/startup-pane.sh"
cat > "$PANE_SH" <<'EOF'
#!/bin/sh
echo "marspot startup gauge"
PS1='$ ' exec /bin/sh
EOF
chmod +x "$PANE_SH"

write_layout() {
  python3 - "$MARSPOT_STATE_DIR" <<'PY'
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1])
def s(x): b = x.encode(); return struct.pack('<H', len(b)) + b
body = struct.pack('<II', 0xA5505010, 2) + struct.pack('<HH', 0, 1)
body += struct.pack('<HHHH', 1, 1, 0, 1) + struct.pack('<Q', 0) + s('') + s('')
(d / 'shell-state.bin').write_bytes(body)
frames = struct.pack('<IIH', 0xA5505011, 2, 1) + struct.pack('<Idddd', 0, 60.0, 400.0, 736.0, 640.0)
(d / 'window-state.bin').write_bytes(frames)
PY
}

load1() { sysctl -n vm.loadavg | awk '{print $2}'; }

# one_launch DIR -> prints "<ms> <load1> <boot breakdown>"
one_launch() {
  local dir=$1 t0 t1 l
  dev_kill_shell_core
  dev_wipe_state
  rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired"
  mkdir -p "$MARSPOT_STATE_DIR/logs"
  write_layout || fail "could not write the layout"
  l=$(load1)
  env SHELL="$PANE_SH" MARSPOT_SESSION_BIN="$dir/marspot-session" MARSPOT_CORE_BIN="$dir/marspot-core" \
    nohup "$GAUGE" exec "$MARSPOT_STATE_DIR/launch.ms" "$dir/marspot-shell" \
    > "$(dev_run_log startup)" 2>&1 < /dev/null &
  dev_record_pid $!
  disown
  t1=$("$GAUGE" wait-event "$APPLOG" WINDOW_FIRST_CONTENT 180) || fail "the pane's output never reached the screen"
  t0=$(cat "$MARSPOT_STATE_DIR/launch.ms")
  echo "$((t1 - t0)) $l $("$GAUGE" boot "$APPLOG")"
}

READINGS="$MARSPOT_STATE_DIR/startup-readings.txt"
: > "$READINGS"
echo "==> $TRIALS trials, load1 $(load1)"
for (( i = 1; i <= TRIALS; i++ )); do
  rm -rf "${COPIES:?}"
  mkdir -p "$COPIES"
  cp "$BIN/marspot-shell" "$BIN/marspot-core" "$BIN/marspot-session" "$COPIES/"
  # a copy keeps its code hash, and macOS keys what it has already
  # assessed and compiled to that: signed again under an identifier of
  # its own, each copy is a binary this machine has never run
  for b in marspot-shell marspot-core marspot-session; do
    codesign -f -s - -i "com.goliajp.marspot.startup-gauge.$$.$i.$b" "$COPIES/$b" 2>/dev/null \
      || fail "could not sign $COPIES/$b"
  done
  for kind in cold warm; do
    r=$(one_launch "$COPIES") || { echo "$r"; exit 2; }
    echo "$kind $r" >> "$READINGS"
    echo "    trial $i $kind: ${r%% *} ms (load1 $(cut -d' ' -f2 <<<"$r"); from STARTUP: $(cut -d' ' -f3- <<<"$r"))"
    sleep 2
  done
done
dev_kill_shell_core
python3 - "$READINGS" "$OUT" <<'PY'
import json, sys
rows = [l.split() for l in open(sys.argv[1]) if l.strip()]
doc = {}
for kind in ("cold", "warm"):
    xs = [int(r[1]) for r in rows if r[0] == kind]
    loads = [float(r[2]) for r in rows if r[0] == kind]
    s = sorted(xs)
    med = s[len(s) // 2] if len(s) % 2 else (s[len(s) // 2 - 1] + s[len(s) // 2]) / 2
    doc[kind] = {"ms": med, "ms_samples": xs, "load1_start": loads[0], "load1_max": max(loads), "load1_end": loads[-1]}
json.dump(doc, open(sys.argv[2], "w"), indent=1)
for kind in ("cold", "warm"):
    print(f"startup {kind} {doc[kind]['ms']:g}")
PY
