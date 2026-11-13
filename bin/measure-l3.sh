#!/usr/bin/env bash
# bin/measure-l3.sh — what marspot-session costs to absorb each
# cross-terminal cat-* scenario: instructions per byte first, wall-clock
# throughput beside it.
#
#   bin/measure-l3.sh                      one session, all four scenarios
#                                          -> bench/results/l3-throughput.json
#   bin/measure-l3.sh --sessions 9 --scenario cat-mixed
#                                          nine sessions absorbing the same
#                                          scenario at once
#                                          -> bench/results/l3-multi.json
#
# Why this exists: bin/measure.sh drives the standalone `mcli` binary (one
# in-process session, no IPC/shm hop).  That is not the architecture the
# product ships: every pane's bytes go through its own marspot-session
# process (parser → grid → shm publish).
#
# What it drives: the `l3_throughput` example spawns marspot-session
# directly and feeds it the scenario.  The shell and the core are not on
# this path, so only a change to marspot-session or marspot-term can move
# these numbers -- worth knowing before reading a red row as a regression
# in something else.
#
# Why instructions: the wall clock of one unchanged build read 101.7 to
# 139.8 MB/s on cat-emoji across six runs in one day while the session
# retired 209 to 213 instructions per byte in every one of them.  The
# session was not doing more work in the slow runs, it was waiting.  A
# count of the work is what a change to the code moves, so that is what
# the gate judges; the wall clock is reported with its spread.
#
# With --sessions N a trial starts N probes together.  Instructions and
# bytes are summed over the sessions; the wall clock is the slowest
# session's, so the rate is what the N of them absorbed together.
#
# A trial taken while the host was busy is not a slower reading, it is
# not a reading: it is recorded as invalid and run again, up to REQUEUE
# extra trials per scenario.  bin/_l3_aggregate.py holds the thresholds.
#
# It runs in a sandbox of its own (its own MARSPOT_STATE_DIR); it never
# touches the installed app.
#
# Each scenario file is `cat`-ed REPEATS times in one command so the drain
# window clears the 10 ms resolution of the trial's clock (a 32 MiB file
# drains in ~0.2 s — too short to time honestly).  Repeating the path
# keeps the content mix identical; bytes credited = REPEATS × file_bytes.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=/dev/null
source "$ROOT/bin/_lib.sh"          # marspot_bin → CARGO_TARGET_DIR-aware paths
# shellcheck source=/dev/null
source "$ROOT/bin/_dev-sandbox.sh"

# No shelld here: RFC-003 retired L4, and the probe spawns the L3 it
# measures in a sandbox of its own.  The two lines that used to resolve
# a shelld binary are gone with it — `marspot_bin marspot-shelld` now
# names a file that is never built, which is exactly the kind of
# leftover that kept this script "running" while every trial failed.

SCENARIOS_DIR="$ROOT/bench/scenarios"
RESULTS_DIR="$ROOT/bench/results"
SESSION_BIN="$(marspot_bin marspot-session)"
PROBE_BIN="$(dirname "$SESSION_BIN")/examples/l3_throughput"
mkdir -p "$RESULTS_DIR"

fail() { echo "measure-l3: $*" >&2; exit 1; }

SESSIONS=1
SCENARIOS=(cat-ascii cat-mixed cat-cjk cat-emoji)
PICKED=()
while (( $# )); do
  case "$1" in
    --sessions) SESSIONS="${2:?--sessions N}"; shift 2 ;;
    --scenario) PICKED+=("${2:?--scenario ID}"); shift 2 ;;
    -h|--help)  sed -n '2,18p' "$0"; exit 0 ;;
    *)          fail "unknown arg: $1" ;;
  esac
done
[[ $SESSIONS =~ ^[1-9][0-9]*$ ]] || fail "--sessions wants a count, got '$SESSIONS'"
(( ${#PICKED[@]} == 0 )) || SCENARIOS=("${PICKED[@]}")
if (( SESSIONS == 1 )); then
  OUT_JSON="$RESULTS_DIR/l3-throughput.json"
else
  OUT_JSON="$RESULTS_DIR/l3-multi.json"
fi
# a result left by an earlier run must not be read as this one's
rm -f "$OUT_JSON"

# Started over ssh, this has to run while that ssh session is still
# open.  One build read 170-171 instructions per byte on cat-mixed when
# the session that started it had ended and 178-182 while it was open,
# minutes apart and repeatedly, with the same environment, the same nice
# value and the same bytes written to scrollback and bytelog -- and it
# is the session being open that decides, not whose child the probe is:
# a probe detached with nohup under an open session reads the higher
# level too.  Why is not known.  The ceilings in bench/baseline.json
# were measured with the session open, which is how bin/bench-remote.sh
# runs this, so the other level is refused rather than compared.
if [[ -n "${SSH_CONNECTION:-}" ]]; then
  _p=$$
  _under_sshd=0
  while [[ -n "$_p" && "$_p" != "0" && "$_p" != "1" ]]; do
    case "$(ps -o comm= -p "$_p" 2>/dev/null)" in
      *sshd*) _under_sshd=1; break ;;
    esac
    _p="$(ps -o ppid= -p "$_p" 2>/dev/null | tr -d ' ')"
  done
  (( _under_sshd )) || fail "the ssh session that started this has ended; the instruction count is 4-6 % lower in that state than the ceilings were measured in.  Run it with the session kept open."
fi

# More than three.  Trials are lottery tickets for an uncontended window
# on a host that has a permanent tenant.  Measured 2026-08-19: with
# three, one batch came back 28.7 / 72.3 / 160.0 MiB/s for the same
# scenario — it took all three to find one clean run — and the next
# batch missed entirely on cjk.  Seven since the instruction count is
# what is judged: it is the median of the trials, and seven is what the
# ceilings in bench/baseline.json were measured with.
TRIALS="${TRIALS:-7}"
# Extra trials a scenario may run to replace the ones the host spoiled.
REQUEUE="${REQUEUE:-$TRIALS}"
# Target drain payload per session and trial.  ~128 MiB → ~0.7-1.3 s on
# Apple Silicon: long enough that the 10 ms clock resolves it to ~1 %,
# short enough to keep the whole gate run to a few minutes.
TARGET_MIB="${TARGET_MIB:-128}"

# Build session bin + probe together (version-skew guard: a stale probe
# would create a grid_shm region an updated marspot-session refuses).
# The build's exit code decides, not whether a binary is lying in target/.
dev_require_probe l3_throughput || fail "could not build the probe"
dev_require_bin marspot-session || fail "could not build marspot-session"
[[ -x "$SESSION_BIN" ]] || fail "marspot-session not built at $SESSION_BIN"
[[ -x "$PROBE_BIN" ]]   || fail "probe not built at $PROBE_BIN"

# After every trial, make sure its session is not still holding a pty.
# One failed trial's orphan used to tax every trial after it until the
# next failure swept it -- measured 2026-10-02: four of ten trials
# reading 29-59 s against a normal 1.3 s, with twice the instructions and
# forty times the wakeups, all of it our own leftovers competing for the
# host.  The probe runs its session in a sandbox named after its own pid
# ($TMPDIR/l3-throughput-<pid>) and removes it when it ends cleanly; a
# sandbox still there holds the registry entry of the session it
# started, which is the one pid this stops.
reap_trial() {
  # std::env::temp_dir: TMPDIR, else the per-user temp dir
  local sandbox="${TMPDIR:-$(getconf DARWIN_USER_TEMP_DIR 2>/dev/null || echo /tmp)}"
  sandbox="${sandbox%/}/l3-throughput-$1"
  [[ -d $sandbox ]] || return 0
  ( MARSPOT_STATE_DIR="$sandbox"; dev_kill_shell_core )
  rm -rf "$sandbox"
}

load1() { sysctl -n vm.loadavg | awk '{print $2}'; }

# One trial: SESSIONS probes started together, each with a session of
# its own.  Leaves in $RUN_DIR what bin/_l3_aggregate.py judges it by:
# probe.<i>.out (the probe's reading), probe.<i>.err (its witness line,
# what the session actually wrote), trial.load (the load average before,
# every second during, and after) and a CPU snapshot of every process on
# the host from either side of it.
#
# The load is sampled through the trial and not once before it: a
# reading taken before the first trial says nothing about what arrives
# during the next minutes.  2026-10-01 this probe recorded 4.16 while
# two jobs started a minute in and ran to the end, and reported a 45%
# "regression" on cjk that was entirely the tenant.
run_trial() {
  local scenario_path=$1 repeats=$2 i p pids=() sampler
  rm -f "$RUN_DIR"/probe.*.out "$RUN_DIR"/probe.*.err
  load1 > "$RUN_DIR/trial.load"
  python3 "$ROOT/bin/_bench_record.py" cpu-snapshot > "$RUN_DIR/cpu.before"
  ( while sleep 1; do load1; done ) >> "$RUN_DIR/trial.load" 2>/dev/null &
  sampler=$!
  echo "$sampler" > "$RUN_DIR/live-pids"
  for ((i = 0; i < SESSIONS; i++)); do
    "$PROBE_BIN" "$SESSION_BIN" "$scenario_path" "$repeats" \
      > "$RUN_DIR/probe.$i.out" 2> "$RUN_DIR/probe.$i.err" &
    pids+=($!)
    echo "$!" >> "$RUN_DIR/probe-pids"
    echo "$!" >> "$RUN_DIR/live-pids"
  done
  for p in "${pids[@]}"; do
    wait "$p"
    reap_trial "$p"
  done
  kill "$sampler" 2>/dev/null
  wait "$sampler" 2>/dev/null
  : > "$RUN_DIR/live-pids"
  # which cores the sessions and the programs feeding them ran on
  grep -h '^placement:' "$RUN_DIR"/probe.*.err 2>/dev/null | sed 's/^/    /'
  python3 "$ROOT/bin/_bench_record.py" cpu-snapshot > "$RUN_DIR/cpu.after"
  load1 >> "$RUN_DIR/trial.load"
}

RUN_DIR=$(mktemp -d)
cleanup() {
  local p
  # stopped mid-trial: the load sampler and the probes of that trial
  # are still running and would outlive this script
  for p in $(cat "$RUN_DIR/live-pids" 2>/dev/null); do kill "$p" 2>/dev/null; done
  for p in $(cat "$RUN_DIR/probe-pids" 2>/dev/null); do reap_trial "$p"; done
  rm -rf "$RUN_DIR"
}
trap cleanup EXIT
trap 'exit 143' TERM INT HUP

for scenario in "${SCENARIOS[@]}"; do
  spath="$SCENARIOS_DIR/$scenario.bin"
  [[ -f "$spath" ]] || { echo "==> missing $spath — run bin/gen-scenarios.sh" >&2; continue; }
  fbytes=$(stat -f%z "$spath")
  # ceil(TARGET_MIB MiB / file size), at least 1
  repeats=$(python3 -c "import math; print(max(1, math.ceil($TARGET_MIB*1048576/$fbytes)))")
  total_bytes=$((fbytes * repeats))

  echo "==> $scenario ×$repeats ($((total_bytes / 1000000)) MB) into $SESSIONS session(s)"
  python3 "$ROOT/bin/_bench_record.py" lock-waiters > "$RUN_DIR/$scenario.waiters"
  valid=0
  attempt=0
  while (( valid < TRIALS && attempt < TRIALS + REQUEUE )); do
    attempt=$((attempt + 1))
    echo "  trial $attempt ($valid of $TRIALS valid so far)"
    (( attempt > 1 )) || python3 "$ROOT/bin/_l3_aggregate.py" settle 240 3.0
    run_trial "$spath" "$repeats"
    if python3 "$ROOT/bin/_l3_aggregate.py" trial "$RUN_DIR" "$scenario" "$total_bytes" "$SESSIONS"; then
      valid=$((valid + 1))
    fi
  done
done

python3 "$ROOT/bin/_l3_aggregate.py" finish "$RUN_DIR" "$OUT_JSON" "$SESSIONS" "${SCENARIOS[@]}" \
  || fail "could not write $OUT_JSON"

echo
echo "==> $OUT_JSON"
python3 "$ROOT/bin/_l3_aggregate.py" table "$OUT_JSON"
