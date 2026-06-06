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
#   - exclusive : mkdir lock both ends (atomic; macOS has no flock(1))
#   - clean     : dedicated CARGO_TARGET_DIR, caffeinate -dims
#   - terminating: trap on EXIT/INT/TERM kills the remote PGID + sweeps
#                  any orphan mars/mcli; lock released unconditionally
#   - bounded   : cargo sweep --time 30 prunes stale target/ artefacts
#   - retrievable: bench/results/ rsync'd back to bench/remote-runs/<ts>/
#
# The remote shell pid is captured into $REMOTE_PIDF; SSH-spawned shells
# are session/PG leaders so $$ == PGID, which lets `kill -- -PGID` collect
# every descendant including the bench.sh subshells and mcli children.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOST="${HOST:-mini}"
REMOTE_DIR="${REMOTE_DIR:-bench-marspot}"
LOCK_LOCAL="/tmp/marspot-bench-remote.lock.d"
REMOTE_LOCK="/tmp/marspot-bench-remote.lock.d"
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
    pkill -x mars 2>/dev/null || true
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

# ---- sync ------------------------------------------------------------
# `--delete` to keep the remote a faithful mirror of the dev tree, but
# excluded paths (target/, results/, generated scenarios) are *not*
# deleted on the remote even though they are absent locally — rsync's
# default behaviour without --delete-excluded.
echo "==> rsync → $HOST:~/$REMOTE_DIR/"
rsync -a --delete --quiet \
  --exclude '/target/' \
  --exclude '/build/' \
  --exclude '/references/*' --include '/references/README.md' \
  --exclude '/bench/remote-runs/' \
  --exclude '/bench/results/' \
  --exclude '/bench/scenarios/*.bin' \
  --exclude '.DS_Store' \
  --exclude '/.claude/settings.json' \
  "$ROOT/" "$HOST:$REMOTE_DIR/"

# ---- run -------------------------------------------------------------
# Quote args once locally so the remote shell can re-tokenise them
# without word-splitting surprises. Empty $@ must produce empty
# string, not `''` (which bench.sh would reject as "unknown arg").
ARGS_Q=""
if (( $# > 0 )); then
  ARGS_Q="$(printf '%q ' "$@")"
fi
echo "==> running bench on $HOST: bin/bench.sh $ARGS_Q"
set +e
ssh "$HOST" "
  set -e
  cd ~/$REMOTE_DIR
  export CARGO_TARGET_DIR=\$HOME/$REMOTE_DIR/target
  echo \$\$ > $REMOTE_PIDF
  exec caffeinate -dims ./bin/bench.sh $ARGS_Q
" </dev/null
REMOTE_RC=$?
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
