#!/usr/bin/env bash
# bin/bench-remote.sh — run bench.sh on a clean remote Apple Silicon host.
#
# Why: the dev box has jitter (foreground apps, dev sessions, browsers);
# a dedicated idle box gives stable numbers. Same arch + macOS keeps
# the baseline directly comparable.
#
# Usage:
#   bin/bench-remote.sh                # fast tier (default host=mini)
#   bin/bench-remote.sh --full         # full tier
#   HOST=other-mini bin/bench-remote.sh --full
#
# Contract:
#   - exclusive : mkdir lock both ends against other marspot runs, plus
#                 the host's shared bench lock, held exclusively, against
#                 everything else using the machine
#   - clean     : dedicated CARGO_TARGET_DIR, caffeinate -dims
#   - terminating: trap on EXIT/INT/TERM kills the remote PGID + sweeps
#                  any orphan marspot/mcli; lock released unconditionally
#   - bounded   : cargo sweep --time 30 prunes stale target/ artefacts
#   - retrievable: bench/results/ rsync'd back to bench/remote-runs/<ts>/
#
# The remote shell pid is captured into $REMOTE_PIDF; SSH-spawned shells
# are session/PG leaders so $$ == PGID, which lets `kill -- -PGID` collect
# every descendant including the bench.sh subshells and mcli children.

set -euo pipefail

# One tool does the copying, for every session and every repo: it takes
# the exclude list from git itself, so a path this repo ignores is never
# sent and never deleted on the far side -- which is how the remote
# target/ survives a sync from a tree that has none.
remote_sync() {
  local tool
  tool="$(command -v remote-sync || true)"
  [[ -n $tool ]] || tool="$HOME/workspace/goliajp/golia-claude-configs/bin/remote-sync"
  [[ -x $tool ]] || {
    echo "remote-sync not found (looked on PATH and in $tool)" >&2
    exit 1
  }
  "$tool" "$@"
}

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOST="${HOST:-mini}"
REMOTE_NAME="${REMOTE_NAME:-bench-marspot}"
# remote-sync always lands in ~/work/<name>, so that is where the
# run has to look.
REMOTE_DIR="work/$REMOTE_NAME"
LOCK_LOCAL="/tmp/marspot-bench-remote.lock.d"
# One lock for every kind of measuring this host does.  A bench and a
# test run on the same box at the same time do not merely queue — the
# bench measures the test suite's load and calls it a regression
# (2026-09-28: an "idle re-measure" scored parse-file 47% low and
# skipped its L3 rows at load 19.8, because a 14-thread test run had
# started underneath it).  The name is deliberately not "bench".
REMOTE_LOCK="/tmp/marspot-runner.lock.d"
REMOTE_PIDF="/tmp/marspot-bench-remote.pgid"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
OUT_DIR="$ROOT/bench/remote-runs/$TS"

# ---- local lock ------------------------------------------------------
if ! mkdir "$LOCK_LOCAL" 2>/dev/null; then
  if [[ -f "$LOCK_LOCAL/pid" ]] && kill -0 "$(cat "$LOCK_LOCAL/pid")" 2>/dev/null; then
    echo "bench-remote already running locally (pid $(cat "$LOCK_LOCAL/pid"))" >&2
    exit 1
  fi
  echo "==> breaking stale local lock"
  rm -rf "$LOCK_LOCAL" && mkdir "$LOCK_LOCAL"
fi
echo "$$" > "$LOCK_LOCAL/pid"
printf '%s\t%s\n' "$TS" "$HOST" > "$LOCK_LOCAL/info"

# ---- cleanup ---------------------------------------------------------
cleanup() {
  local rc=$?
  echo "==> cleanup"
  ssh -o ConnectTimeout=5 "$HOST" "
    if [[ -f $REMOTE_PIDF ]]; then
      pgid=\$(cat $REMOTE_PIDF 2>/dev/null || true)
      if [[ -n \${pgid:-} ]]; then
        kill -- -\$pgid 2>/dev/null || true
      fi
      rm -f $REMOTE_PIDF
    fi
    pkill -x marspot 2>/dev/null || true
    pkill -x mcli 2>/dev/null || true
    rm -rf $REMOTE_LOCK
  " </dev/null >/dev/null 2>&1 || true
  rm -rf "$LOCK_LOCAL"
  exit $rc
}
trap cleanup EXIT INT TERM

# ---- pre-flight ------------------------------------------------------
echo "==> pre-flight on $HOST"
ssh -o ConnectTimeout=5 "$HOST" "
  set -e
  mkdir -p ~/$REMOTE_DIR
  if ! mkdir $REMOTE_LOCK 2>/dev/null; then
    if [[ -f $REMOTE_LOCK/pid ]] && kill -0 \$(cat $REMOTE_LOCK/pid) 2>/dev/null; then
      echo 'remote lock held by pid '\$(cat $REMOTE_LOCK/pid) >&2; exit 1
    fi
    rm -rf $REMOTE_LOCK && mkdir $REMOTE_LOCK
  fi
  echo \$\$ > $REMOTE_LOCK/pid
  command -v cargo >/dev/null || { echo 'cargo missing on remote' >&2; exit 2; }
" </dev/null

# Load: reported, never a refusal.
#
# This used to refuse past load 20 and skip individual checks past 5,
# and between the two a busy fleet meant no perf measurement at all --
# ten checks went unmeasured for as long as the machines had company,
# which is most of the time. What that costs compounds: a regression
# nobody measures is a regression nobody finds, and the next one lands
# on top of it.
#
# The asymmetry is what makes measuring anyway safe, and it runs one
# way only: company can make a number worse, never better. So a PASS
# carrying that handicap is a real pass and worth having, and a FAIL
# is the one verdict that cannot be read -- the gate marks those
# UNSURE by itself (`check` in bin/bench.sh), so no busy run can
# produce a false regression.
#
# What is left here is telling the reader the conditions, and who the
# company was.
if [[ "${MARSPOT_BENCH_IGNORE_LOAD:-}" != "1" ]]; then
  LOAD1="$(ssh "$HOST" "sysctl -n vm.loadavg | awk '{print \$2}'" </dev/null)"
  if python3 -c "import sys; sys.exit(0 if float('$LOAD1') > 5.0 else 1)"; then
    echo "==> $HOST has company (load1=$LOAD1).  Measuring anyway: a PASS is" >&2
    echo "    valid under load, and a FAIL comes back UNSURE rather than as a" >&2
    echo "    regression.  Re-run on a quiet machine to settle those." >&2
    ssh "$HOST" "ps aux | sort -k3 -rn | head -3 | awk '{printf \"    %s%% %s\n\", \$3, \$11}'" </dev/null >&2 || true
  elif python3 -c "import sys; sys.exit(0 if float('$LOAD1') > 1.5 else 1)"; then
    echo "==> note: $HOST under light load (load1=$LOAD1); numbers read low." >&2
  fi
fi

# ---- sync ------------------------------------------------------------
# `--delete` to keep the remote a faithful mirror of the dev tree, but
# excluded paths (target/, results/, generated scenarios) are *not*
# deleted on the remote even though they are absent locally — rsync's
# default behaviour without --delete-excluded.
remote_sync "$HOST" --name "$REMOTE_NAME"

# ---- run -------------------------------------------------------------
# Quote args once locally so the remote shell can re-tokenise them
# without word-splitting surprises. Empty $@ must produce empty
# string, not `''` (which bench.sh would reject as "unknown arg").
ARGS_Q=""
if (( $# > 0 )); then
  ARGS_Q="$(printf '%q ' "$@")"
fi
echo "==> running bench on $HOST: bin/bench.sh $ARGS_Q"
# Forward the stale-competitors bypass so users can `--full` without
# a fresh competitors_snapshot when the GUI-AppleEvent workaround
# isn't available (see bin/remote-measure-others.sh).
STALE_PASS=""
[[ "${MARSPOT_BENCH_ALLOW_STALE_COMPETITORS:-}" == "1" ]] && \
  STALE_PASS="export MARSPOT_BENCH_ALLOW_STALE_COMPETITORS=1;"
# The measurement holds the machine's bench lock, exclusively, for as
# long as it runs.
#
# That lock is how everything on this host says "I am using the CPU":
# builds, test runs and batch jobs take it shared and run alongside each
# other, a measurement takes it exclusively and therefore waits for all
# of them.  We were not taking it at all -- the header used to say macOS
# has no flock(1), which stopped being true when Homebrew's went on the
# runner -- so the gate measured straight through whatever else was
# running.  On 2026-10-01 that was a research job holding three cores
# for three hours, and the same scenario read 218 MB/s and 142 MB/s
# forty minutes apart, one side of the gate's floor and then the other.
# A number taken while someone else has the CPU is not a slower number,
# it is not a number.
#
# `/usr/local/bin/bench-lock bench` rather than a bare exclusive flock: a batch job
# holding the shared lock for hours made an exclusive one unobtainable
# -- not slow, unobtainable -- and both this and another repo's bench
# ended up going around the lock, which is how one of them got
# mistaken for a stray process and killed. It blocks new heavy work and
# waits only for what is already running.
# The absolute path on purpose: a non-login ssh does not have
# /usr/local/bin on its PATH, and the failure looks like "bench-lock:
# not found" from a tool that is installed.
# How long this is willing to wait for the machine.
#
# A waiting benchmark holds every new heavy job back, so one that
# waits and then gives up has blocked them for nothing: on 2026-10-01
# this one waited 91 minutes behind a long batch, timed out, and the
# six jobs queued behind it all took the lock the second it vanished.
# Saying the number out loud is the fix -- how long we can wait is
# something only this side knows.
MAX_WAIT="${MARSPOT_BENCH_MAX_WAIT:-1200}"
LOCK_CMD="/usr/local/bin/bench-lock bench --max-wait $MAX_WAIT"
if [[ "${MARSPOT_BENCH_NO_LOCK:-}" == "1" ]]; then
  echo "==> MARSPOT_BENCH_NO_LOCK=1 — measuring without the host lock" >&2
  LOCK_CMD=""
fi
set +e
ssh "$HOST" "
  set -e
  cd ~/$REMOTE_DIR
  export CARGO_TARGET_DIR=\$HOME/$REMOTE_DIR/target
  $STALE_PASS
  echo \$\$ > $REMOTE_PIDF
  if [ -n \"$LOCK_CMD\" ] && [ ! -x /usr/local/bin/bench-lock ]; then
    echo 'bench-remote: /usr/local/bin/bench-lock is not on the runner' >&2
    exit 1
  fi
  # One hold, covering the compile and the measurement together.
  #
  # The compile was split out under the shared 'heavy' mode first,
  # on the reasoning
  # that a build is not a measurement and should not make everyone
  # wait -- true of one bench on a quiet machine, wrong
  # with two. A waiting bench holds new heavy work back, so the other
  # session's queued bench stood in front of this one's compile and
  # the build timed out without ever producing a binary to measure.
  # Queueing the whole thing as one bench puts it in the bench queue
  # instead, where it waits behind benches rather than behind the
  # consequences of them. The cost is holding the exclusive lock
  # across a compile, which is the trade the global rule takes.
  exec $LOCK_CMD caffeinate -dims sh -c \
    './bin/bench.sh --build-only && exec ./bin/bench.sh --no-build $ARGS_Q'
" </dev/null
REMOTE_RC=$?
# 75 is bench-lock giving up: the machine was busy, not the code.  A
# different thing to report and a different thing to do about it.
if (( REMOTE_RC == 75 )); then
  echo "==> could not get $HOST to itself within ${MAX_WAIT}s — nothing was measured." >&2
  echo "    Who was in the way is above.  Raise MARSPOT_BENCH_MAX_WAIT, or wait" >&2
  echo "    for them and try again; this is not a gate failure." >&2
fi
set -e

# ---- retrieve --------------------------------------------------------
mkdir -p "$OUT_DIR"
echo "==> retrieving → $OUT_DIR"
rsync -a --quiet "$HOST:$REMOTE_DIR/bench/results/" "$OUT_DIR/" 2>/dev/null || true

# ---- bounded growth: prune remote target/ ----------------------------
ssh "$HOST" "cd ~/$REMOTE_DIR && cargo sweep --time 30 2>&1 | tail -2" \
  </dev/null || true

echo "==> done (bench exit $REMOTE_RC), results in $OUT_DIR"
exit $REMOTE_RC
