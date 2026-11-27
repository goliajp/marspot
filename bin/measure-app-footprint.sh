#!/usr/bin/env bash
# bin/measure-app-footprint.sh — the memory the shipped three processes
# hold with one window of 12 panes (4 x 3), once they are idle.
#
# One sandbox app per round: shell, core and twelve sessions from
# target/release; every pane prints a screen and sits at a prompt.
# Once all twelve pane shells run, a frame is on screen and the
# processes have gone idle, the phys_footprint of the shell, the core
# and every session is read and summed.
#
#   FP_ROUNDS   rounds (default 3)
#   FP_OUT      JSON written here (default build/app-footprint.json)
#
# Opens a window and is a measurement, so it runs on the runner, inside
# the console session, alone:
#   E2E_LOCK=bench REMOTE_NAME=<own name> bin/test-e2e-remote.sh measure-app-footprint.sh
# bin/bench.sh --full runs it the same way.
set -uo pipefail
trap 'exit 1' HUP INT TERM

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
dev_no_dock

ROUNDS="${FP_ROUNDS:-3}"
OUT="${FP_OUT:-$ROOT/build/app-footprint.json}"
BIN="${MARSPOT_BENCH_BIN_DIR:-$ROOT/target/release}"   # a bench run passes its copies
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
GAUGE="$ROOT/bin/_app_gauge.py"
COLS=4 ROWS=3
N=$((COLS * ROWS))

fail() {
  echo "FAIL: $*"
  tail -20 "$APPLOG" 2>/dev/null | sed 's/^/    /'
  exit 2
}
trap dev_kill_shell_core EXIT
for b in marspot-shell marspot-core marspot-session; do
  [[ -x $BIN/$b ]] || fail "$BIN/$b is not built"
done
mkdir -p "$MARSPOT_STATE_DIR" "$(dirname "$OUT")"

PANE_SH="$MARSPOT_STATE_DIR/fp-pane.sh"
cat > "$PANE_SH" <<'EOF'
#!/bin/bash
for i in $(seq 1 40); do printf '%03d the quick brown fox jumps over the lazy dog 0123456789\n' "$i"; done
PS1='$ ' exec /bin/sh
EOF
chmod +x "$PANE_SH"

write_layout() {
  python3 - "$MARSPOT_STATE_DIR" "$COLS" "$ROWS" 1400 900 <<'PY'
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1]); cols, rows = int(sys.argv[2]), int(sys.argv[3])
def s(x): b = x.encode(); return struct.pack('<H', len(b)) + b
def pane(): return struct.pack('<Q', 0) + s('') + s('')
n = cols * rows
body  = struct.pack('<II', 0xA5505010, 2)
body += struct.pack('<HH', 0, 1)
body += struct.pack('<HHHH', cols, rows, 0, n) + b''.join(pane() for _ in range(n))
(d / 'shell-state.bin').write_bytes(body)
frames  = struct.pack('<IIH', 0xA5505011, 2, 1)
frames += struct.pack('<Idddd', 0, 40.0, 60.0, float(sys.argv[4]), float(sys.argv[5]))
(d / 'window-state.bin').write_bytes(frames)
PY
}

load1() { sysctl -n vm.loadavg | awk '{print $2}'; }
READINGS="$MARSPOT_STATE_DIR/fp-readings.jsonl"
: > "$READINGS"
echo "==> $N panes, $ROUNDS rounds, load1 $(load1)"
for (( round = 1; round <= ROUNDS; round++ )); do
  dev_kill_shell_core
  dev_wipe_state
  rm -rf "$MARSPOT_STATE_DIR/sessions" "$MARSPOT_STATE_DIR/retired"
  mkdir -p "$MARSPOT_STATE_DIR/logs"
  write_layout || fail "could not write the layout"
  l0=$(load1)
  env SHELL="$PANE_SH" MARSPOT_SESSION_BIN="$BIN/marspot-session" MARSPOT_CORE_BIN="$BIN/marspot-core" \
    nohup "$BIN/marspot-shell" > "$(dev_run_log fp)" 2>&1 < /dev/null &
  dev_record_pid $!
  disown
  for (( i = 0; i < 600; i++ )); do
    (( $(dev_pane_shell_pids | grep -c .) >= N )) && break
    sleep 0.2
  done
  (( $(dev_pane_shell_pids | grep -c .) >= N )) || fail "round $round: $N panes did not come up"
  "$GAUGE" wait-event "$APPLOG" WINDOW_FIRST_PRESENT 60 >/dev/null || fail "round $round: no frame on screen"
  shell=$(dev_shell_pid)
  core=$(dev_core_pids | head -1)
  sessions=$(dev_session_pids)
  (( $(grep -c . <<<"$sessions") == N )) || fail "round $round: $(grep -c . <<<"$sessions") sessions, wanted $N"
  [[ -n $shell && -n $core ]] || fail "round $round: no shell or core"
  # shellcheck disable=SC2086
  r=$("$GAUGE" idle-footprint "$shell" "$core" $sessions) || fail "round $round: footprint read failed"
  l1=$(load1)
  python3 - "$r" "$shell" "$core" "$l0" "$l1" "$round" >> "$READINGS" <<'PY'
import json, sys
r = json.loads(sys.argv[1]); shell, core = sys.argv[2], sys.argv[3]
fp = r["footprint"]
out = {"round": int(sys.argv[6]), "sum": r["sum"], "shell": fp[shell], "core": fp[core],
       "sessions": [v for k, v in fp.items() if k not in (shell, core)],
       "idle": r["idle"], "idle_cores": r["idle_cores"], "waited_s": r["waited_s"],
       "load1_start": float(sys.argv[4]), "load1_end": float(sys.argv[5])}
print(json.dumps(out))
m = lambda b: b / 1048576
print(f"    round {out['round']}: sum {m(out['sum']):.1f} MiB = shell {m(out['shell']):.1f} + core {m(out['core']):.1f}"
      f" + {len(out['sessions'])} sessions {m(sum(out['sessions'])):.1f} (each {m(min(out['sessions'])):.2f}-"
      f"{m(max(out['sessions'])):.2f}); idle {out['idle_cores']} cores after {out['waited_s']}s, load1 {sys.argv[4]}->{sys.argv[5]}",
      file=sys.stderr)
PY
  [[ ${PIPESTATUS[0]} == 0 ]] || fail "round $round: no reading"
done
dev_kill_shell_core
python3 - "$READINGS" "$OUT" <<'PY'
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
s = sorted(r["sum"] for r in rows)
doc = {"panes": 12, "sum_bytes": s[len(s) // 2], "sum_bytes_samples": [r["sum"] for r in rows],
       "all_idle": all(r["idle"] for r in rows),
       "load1_start": rows[0]["load1_start"], "load1_end": rows[-1]["load1_end"],
       "load1_max": max(max(r["load1_start"], r["load1_end"]) for r in rows), "rounds": rows}
json.dump(doc, open(sys.argv[2], "w"), indent=1)
print(f"footprint12 {doc['sum_bytes']} bytes (median of {len(rows)}: {', '.join(str(x) for x in doc['sum_bytes_samples'])})")
PY
