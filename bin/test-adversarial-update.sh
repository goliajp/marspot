#!/usr/bin/env bash
#
# Adversarial silent-update test — robustness edges beyond the signature
# trust gate (which test-negative-update.sh owns):
#
#   1. equal-version feed: a release tagged at the SAME version as the
#      running binary must be a silent no-op — the updater polls the
#      feed but never downloads the tarball or stages anything.  (The
#      is_newer_than spectrum, incl. downgrade, is unit-tested in
#      updater.rs::version_ordering; this proves the gate is wired into
#      check_and_stage at integration level.)  A regression that
#      mis-ordered versions would re-stage the same release forever.
#
#   2. concurrent triggers: firing several --trigger (SIGUSR1) in rapid
#      succession with one pending staged must apply EXACTLY ONCE — no
#      double-spawn, no tree corruption, one live core at the end.
#
# Sandbox-only; never touches the installed app.  Run after
# `cargo build --release`.  Port 6026 is this project's registry
# allocation for the adversarial feed.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SUP_LOG="$MARSPOT_STATE_DIR/logs/supervisor.log"
TREE="$MARSPOT_STATE_DIR/binaries"
RUN_LOG=/tmp/marspot-test-adversarial.log
PORT=6026
ASSET=marspot-aarch64-apple-darwin.tar.gz
SERVE_DIR=$(mktemp -d /tmp/marspot-advfeed.XXXXXX)
ACCESS_LOG="$SERVE_DIR/access.log"
HTTP_PID=

fail() {
  echo "FAIL: $*"
  echo "  (supervisor tail):"; tail -15 "$SUP_LOG" 2>/dev/null | sed 's/^/    /'
  echo "  (shell run log tail):"; tail -10 "$RUN_LOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}
cleanup() {
  [[ -n "$HTTP_PID" ]] && kill "$HTTP_PID" >/dev/null 2>&1
  dev_kill_shell_core
  rm -rf "$SERVE_DIR"
}
trap cleanup EXIT

RUNNING_VER=$("$SHELL_BIN" --version 2>/dev/null | awk '{print $2}')
[[ -n "$RUNNING_VER" ]] || fail "could not read running version"

# ====================================================================
# Case 1: equal-version feed → poll happens, no download, no stage.
# ====================================================================
[[ -f "$ROOT/keys/marspot-update.sec" ]] \
  || fail "no keys/marspot-update.sec — needed to build the feed tarball"
"$ROOT/bin/build-release-tarball.sh" --output "$SERVE_DIR/$ASSET" --sign >/dev/null \
  || fail "build-release-tarball.sh --sign failed"
# Tag EQUAL to the running version — a correctly-wired gate ignores it.
# A real tarball + sig are served so a broken gate would actually
# download + stage (making the failure observable), not just error out.
cat > "$SERVE_DIR/feed.json" <<EOF
{
  "tag_name": "v$RUNNING_VER",
  "assets": [
    { "name": "$ASSET",     "browser_download_url": "http://127.0.0.1:$PORT/$ASSET" },
    { "name": "$ASSET.sig", "browser_download_url": "http://127.0.0.1:$PORT/$ASSET.sig" }
  ]
}
EOF

python3 -m http.server "$PORT" --bind 127.0.0.1 -d "$SERVE_DIR" >"$ACCESS_LOG" 2>&1 &
HTTP_PID=$!
disown
sleep 0.5
curl -sf "http://127.0.0.1:$PORT/feed.json" >/dev/null \
  || fail "feed server didn't come up on :$PORT"

dev_ensure_shelld || fail "sandbox shelld"
dev_kill_shell_core
dev_wipe_state
mkdir -p "$(dirname "$SUP_LOG")"
: > "$SUP_LOG"; : > "$RUN_LOG"
MARSPOT_UPDATE_FEED="http://127.0.0.1:$PORT/feed.json" \
  nohup "$SHELL_BIN" >"$RUN_LOG" 2>&1 < /dev/null &
disown
for _ in $(seq 1 50); do grep -q HELLO_ACK "$SUP_LOG" 2>/dev/null && break; sleep 0.1; done
grep -q HELLO_ACK "$SUP_LOG" 2>/dev/null || fail "[equal] boot HelloAck never landed"
echo "[equal] booted at v$RUNNING_VER against an equal-version feed"

# The updater's first poll fires ~30 s after launch.  Wait for the
# server to log the feed GET — positive proof the poll ran — then assert
# the tarball was NEVER fetched and nothing was staged.
START=$(date +%s)
until grep -q "GET /feed.json" "$ACCESS_LOG" 2>/dev/null; do
  if (( $(date +%s) - START > 50 )); then
    fail "[equal] updater never polled the feed within 50 s"
  fi
  sleep 1
done
# Give a couple seconds for a (mis)behaving updater to act on it.
sleep 3
grep -q "GET /$ASSET" "$ACCESS_LOG" 2>/dev/null \
  && fail "[equal] updater DOWNLOADED the tarball for an equal version — version gate broken"
for b in marspot-core marspot-shell marspot-shelld; do
  [[ -e "$TREE/pending/$b" ]] && fail "[equal] updater staged $b for an equal version"
done
grep -q "\[updater\] check failed" "$RUN_LOG" 2>/dev/null \
  && fail "[equal] updater logged an error — an equal version should be a silent no-op, not a failure"
echo "    OK — polled, ignored equal version, no download, nothing staged"

kill "$HTTP_PID" >/dev/null 2>&1; HTTP_PID=
dev_kill_shell_core

# ====================================================================
# Case 2: concurrent triggers → apply exactly once.
# ====================================================================
export MARSPOT_PROBATION_S=3
dev_ensure_shelld || fail "sandbox shelld"
dev_wipe_state
: > "$SUP_LOG"; : > "$RUN_LOG"
nohup "$SHELL_BIN" >"$RUN_LOG" 2>&1 < /dev/null &
disown
for _ in $(seq 1 50); do grep -q HELLO_ACK "$SUP_LOG" 2>/dev/null && break; sleep 0.1; done
grep -q HELLO_ACK "$SUP_LOG" 2>/dev/null || fail "[concurrent] boot HelloAck never landed"

mkdir -p "$TREE/pending"
cp "$CORE_BIN" "$TREE/pending/marspot-core"
# Fire a burst of triggers — the second/third land while the swap from
# the first is in flight.
"$SHELL_BIN" --trigger >/dev/null 2>&1
"$SHELL_BIN" --trigger >/dev/null 2>&1
"$SHELL_BIN" --trigger >/dev/null 2>&1

# Wait for the swap to stabilise.
START=$(date +%s)
until grep -q UPDATE_STABLE "$SUP_LOG" 2>/dev/null; do
  if (( $(date +%s) - START > 30 )); then fail "[concurrent] no UPDATE_STABLE"; fi
  sleep 0.3
done
# Let any duplicate apply attempts settle.
sleep 2
swaps=$(grep -c UPDATE_SWAP "$SUP_LOG" 2>/dev/null)
[[ "$swaps" == "1" ]] || fail "[concurrent] $swaps swaps from one pending (expected exactly 1 — double-apply)"
cores=$(pgrep -f "$TREE/current/marspot-core( |$)" 2>/dev/null | wc -l | tr -d ' ')
[[ "$cores" == "1" ]] || fail "[concurrent] $cores live cores (expected 1)"
[[ ! -f "$TREE/pending/marspot-core" ]] || fail "[concurrent] pending/ not consumed"
[[ ! -f "$TREE/prev/marspot-core" ]] || fail "[concurrent] prev/ not finalized"
echo "[concurrent] OK — 3 rapid triggers applied exactly once, one core, tree clean"

cleanup
trap - EXIT
echo "ALL PASS — equal-version no-op + concurrent-trigger idempotence hold"
