#!/usr/bin/env bash
# bin/test-presenting-footprint.sh — what the shell and the core hold
# while frames are being drawn.
#
# Drawing must not cost the shell anything: it shows the core's surface
# and executes nothing on the GPU, so its footprint while a pane prints
# is its footprint at rest.  A shell that copied frames itself carried
# the graphics driver's pool (over a hundred megabytes) for as long as
# they kept coming.
#
# The core does draw, and pays that pool while it does.  Its size is
# the machine's, not ours, so this measures it here (bin/_gpu_pool_probe.m)
# and holds the core to its own footprint at rest plus the pool plus
# PRESENTING_CORE_OVER_KIB.
#
#   PRESENTING_SHELL_KIB      the shell's bound while presenting
#   PRESENTING_CORE_OVER_KIB  what the core may hold over rest + pool
#
# One sandbox app, one pane printing a line every 50 ms for 12 s, read
# 6 s in and again 4 s after it stops.  Opens a real window:
# bin/test-e2e-remote.sh.  Exits 1 over a bound, 2 with no reading.
set -uo pipefail
trap 'exit 1' HUP INT TERM

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

BIN="$ROOT/target/release"
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
RUNLOG="$(dev_run_log test-presenting-footprint)"
# 27 MiB measured presenting and at rest (2026-11-14, M4 Pro)
SHELL_KIB="${PRESENTING_SHELL_KIB:-36000}"
CORE_OVER_KIB="${PRESENTING_CORE_OVER_KIB:-16000}"

fail() {
  echo "FAIL: $*"
  tail -20 "$APPLOG" 2>/dev/null | cut -c1-300 | sed 's/^/    /'
  exit 2
}
trap dev_kill_shell_core EXIT

for b in marspot-shell marspot-core marspot-session; do
  [[ -x $BIN/$b ]] || fail "$BIN/$b is not built"
done

footprint_kib() { # pid
  python3 -c 'import ctypes, sys
buf = (ctypes.c_uint64 * 64)()
ctypes.CDLL("/usr/lib/libSystem.B.dylib").proc_pid_rusage(int(sys.argv[1]), 4, ctypes.byref(buf)) == 0 or sys.exit(1)
print(buf[9] // 1024)' "$1"
}

dev_kill_shell_core
dev_wipe_state
mkdir -p "$MARSPOT_STATE_DIR/logs"
PROBE="$MARSPOT_STATE_DIR/gpu-pool-probe"
clang -fobjc-arc -framework Metal -framework Foundation "$ROOT/bin/_gpu_pool_probe.m" -o "$PROBE" 2>/dev/null \
  || fail "the pool probe did not compile"
pool_mb="$("$PROBE")" || fail "the pool probe did not run"
echo "==> $(sw_vers -productVersion) $(sw_vers -buildVersion), $(sysctl -n machdep.cpu.brand_string), load1 $(sysctl -n vm.loadavg | awk '{print $2}'); the driver's pool here: $pool_mb MB"

PANE_SH="$MARSPOT_STATE_DIR/presenting-pane.sh"
cat > "$PANE_SH" <<EOF
#!/bin/bash
sleep 4
for i in \$(seq 1 240); do
  printf '\033[3%dm%06d\033[0m the quick brown fox jumps over the lazy dog 0123456789\n' \$((i % 7 + 1)) "\$i"
  sleep 0.05
done
: > "$MARSPOT_STATE_DIR/presenting-done"
PS1='\$ ' exec /bin/sh
EOF
chmod +x "$PANE_SH"
rm -f "$MARSPOT_STATE_DIR/presenting-done"
env SHELL="$PANE_SH" MARSPOT_SESSION_BIN="$BIN/marspot-session" MARSPOT_CORE_BIN="$BIN/marspot-core" \
  nohup "$BIN/marspot-shell" >"$RUNLOG" 2>&1 < /dev/null &
dev_record_pid $!
disown
for (( i = 0; i < 300; i++ )); do
  (( $(dev_pane_shell_pids | grep -c .) >= 1 )) && break
  sleep 0.1
done
(( $(dev_pane_shell_pids | grep -c .) >= 1 )) || fail "the pane did not come up"
l1="$(dev_shell_pid)"
l2="$(dev_core_pids | head -1)"
[[ -n $l1 && -n $l2 ]] || fail "no shell or no core"

sleep 10
[[ ! -e $MARSPOT_STATE_DIR/presenting-done ]] || fail "the pane had stopped printing before the reading"
shell_busy="$(footprint_kib "$l1")" || fail "cannot read the shell's footprint"
core_busy="$(footprint_kib "$l2")" || fail "cannot read the core's footprint"
for (( i = 0; i < 150; i++ )); do
  [[ -e $MARSPOT_STATE_DIR/presenting-done ]] && break
  sleep 0.1
done
[[ -e $MARSPOT_STATE_DIR/presenting-done ]] || fail "the pane never stopped printing"
sleep 4
shell_rest="$(footprint_kib "$l1")" || fail "cannot read the shell's footprint"
core_rest="$(footprint_kib "$l2")" || fail "cannot read the core's footprint"
core_bound=$(( core_rest + pool_mb * 1024 + CORE_OVER_KIB ))
echo "    shell: $shell_busy KiB presenting, $shell_rest KiB at rest"
echo "    core:  $core_busy KiB presenting, $core_rest KiB at rest"

rc=0
if (( shell_busy > SHELL_KIB )); then
  echo "FAIL: the shell holds $shell_busy KiB while presenting, bound $SHELL_KIB"
  rc=1
else
  echo "PASS: the shell holds $shell_busy KiB while presenting, bound $SHELL_KIB"
fi
if (( core_busy > core_bound )); then
  echo "FAIL: the core holds $core_busy KiB while presenting, bound $core_bound (rest $core_rest + pool ${pool_mb} MB + $CORE_OVER_KIB)"
  rc=1
else
  echo "PASS: the core holds $core_busy KiB while presenting, bound $core_bound (rest $core_rest + pool ${pool_mb} MB + $CORE_OVER_KIB)"
fi
exit "$rc"
