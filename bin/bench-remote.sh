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

# Load gate — user ruling 2026-07-29: refuse only past load 20.  The
# old 1.5 threshold blocked for hours whenever any sibling project
# compiled on mini (load 6-10), which cost more than the noise it
# avoided.  The asymmetry that makes 20 safe: load only DEPRESSES
# numbers, so a PASS under load is still a PASS (an idle box would
# only score better).  A FAIL under load is the one verdict that
# needs an idle re-run before anyone treats it as a regression — the
# runner prints that warning when it applies.
# MARSPOT_BENCH_IGNORE_LOAD=1 still bypasses entirely.
MARSPOT_BENCH_MAX_LOAD="${MARSPOT_BENCH_MAX_LOAD:-20}"
if [[ "${MARSPOT_BENCH_IGNORE_LOAD:-}" != "1" ]]; then
  LOAD1="$(ssh "$HOST" "sysctl -n vm.loadavg | awk '{print \$2}'" </dev/null)"
  if python3 -c "import sys; sys.exit(0 if float('$LOAD1') > float('$MARSPOT_BENCH_MAX_LOAD') else 1)"; then
    echo "==> $HOST is overloaded (load1=$LOAD1 > $MARSPOT_BENCH_MAX_LOAD) — refusing to bench." >&2
    ssh "$HOST" "ps aux | sort -k3 -rn | head -3 | awk '{printf \"    %s%% %s\n\", \$3, \$11}'" </dev/null >&2 || true
    echo "    Wait for the foreign workload to finish, or set" >&2
    echo "    MARSPOT_BENCH_IGNORE_LOAD=1 to accept polluted numbers." >&2
    exit 3
  fi
  if python3 -c "import sys; sys.exit(0 if float('$LOAD1') > 1.5 else 1)"; then
    echo "==> note: $HOST under load (load1=$LOAD1).  A PASS is valid" >&2
    echo "    (load only depresses numbers); re-run idle before" >&2
    echo "    treating any FAIL as a real regression." >&2
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
BUILD_LOCK="/usr/local/bin/bench-lock heavy --max-wait $MAX_WAIT"
if [[ "${MARSPOT_BENCH_NO_LOCK:-}" == "1" ]]; then
  echo "==> MARSPOT_BENCH_NO_LOCK=1 — measuring without the host lock" >&2
  LOCK_CMD=""
  BUILD_LOCK=""
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
  # Two holds, not one.  A compile is heavy work and belongs beside
  # other heavy work; only the measurement wants the machine to
  # itself.  Held together, everyone else on the runner waited out a
  # three-minute build for a one-minute measurement -- and the build
  # is the part that varies, because a tree that changed has to be
  # rebuilt and a tree that did not is a no-op.
  # If the compile never got the machine there is nothing to measure,
  # so the give-up code travels rather than being stepped over: the
  # first attempt at this let the build abandon its wait and then
  # measured anyway, and came back exit 1 -- a gate failure, which is
  # not what happened.
  if ! $BUILD_LOCK caffeinate -dims ./bin/bench.sh --build-only; then
    rc=\$?
    echo \"bench.sh --build-only did not run (exit \$rc)\" >&2
    exit \$rc
  fi
  exec $LOCK_CMD caffeinate -dims ./bin/bench.sh --no-build $ARGS_Q
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
