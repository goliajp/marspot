#!/usr/bin/env bash
#
# End-to-end REAL release-pipeline test.  Unlike test-update-flow.sh
# (which hand-copies a binary into pending/), this exercises the
# updater's actual network path:
#
#   build signed tarball → fake GitHub feed on localhost → updater
#   polls, downloads tarball + .sig, verifies signature, stages all
#   three binaries → SIGUSR1 applies shell self-update, second
#   trigger applies core.  (There used to be a fourth binary and a
#   fourth step: marspot-shelld stayed pending behind explicit
#   consent.  RFC-003 retired L4 and deleted the target.)
#
# Slow by design: the updater sleeps 30 s before its first poll.
# Budget ~90 s.  Run after `cargo build --release`; exits 0 on
# success.
#
# Port 6024 is this project's allocation in the global port
# registry (~/.claude/port-registry-data.md) — don't change it
# without re-registering.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
SHELL_BIN="$ROOT/target/release/marspot-shell"
SUP_LOG="$MARSPOT_STATE_DIR/logs/marspot.log"
TREE="$MARSPOT_STATE_DIR/binaries"
RUN_LOG=/tmp/marspot-test-real-update.log
PORT=6024
ASSET=marspot-aarch64-apple-darwin.tar.gz
SERVE_DIR=$(mktemp -d /tmp/marspot-feed.XXXXXX)
HTTP_PID=

fail() {
  echo "FAIL: $*"
  echo "  (last 25 supervisor events):"
  tail -25 "$SUP_LOG" 2>/dev/null | sed 's/^/    /'
  echo "  (shell run log tail):"
  tail -10 "$RUN_LOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

cleanup() {
  [[ -n "$HTTP_PID" ]] && kill "$HTTP_PID" >/dev/null 2>&1
  dev_kill_shell_core
  rm -rf "$SERVE_DIR"
}
trap cleanup EXIT

# --- 0. Build signed tarball + fake feed ----------------------------
[[ -f "$ROOT/keys/marspot-update.sec" ]] \
  || fail "no keys/marspot-update.sec — generate it before running (see docs/silent-update.md)"
"$ROOT/bin/build-release-tarball.sh" --output "$SERVE_DIR/$ASSET" --sign >/dev/null \
  || fail "build-release-tarball.sh --sign failed"
# That step ran `cargo build --release`, so the binaries this test is
# about to launch may be brand-new inodes.  Pay their Gatekeeper
# assessment now, before anything is on a clock.
dev_warm_binaries
[[ -f "$SERVE_DIR/$ASSET.sig" ]] || fail "tarball built but no .sig next to it"

# Feed JSON in the GitHub releases/latest shape.  Asset order
# matters: the tarball entry must precede the .sig entry because the
# updater's scraper takes the FIRST occurrence of the asset name
# (and "<asset>" is a substring of "<asset>.sig").
cat > "$SERVE_DIR/feed.json" <<EOF
{
  "tag_name": "v99.99.99",
  "assets": [
    {
      "name": "$ASSET",
      "browser_download_url": "http://127.0.0.1:$PORT/$ASSET"
    },
    {
      "name": "$ASSET.sig",
      "browser_download_url": "http://127.0.0.1:$PORT/$ASSET.sig"
    }
  ]
}
EOF

python3 -m http.server "$PORT" --bind 127.0.0.1 -d "$SERVE_DIR" >/dev/null 2>&1 &
HTTP_PID=$!
disown
sleep 0.5
curl -sf "http://127.0.0.1:$PORT/feed.json" >/dev/null \
  || fail "feed server didn't come up on :$PORT (squatter? check port registry)"
echo "[1/5] feed OK — signed tarball + feed.json served on :$PORT"

# --- 1. Boot with the fake feed --------------------------------------
dev_ensure_shelld || fail "sandbox shelld"
dev_kill_shell_core
dev_wipe_state
mkdir -p "$(dirname "$SUP_LOG")"
rm -f "$SUP_LOG" 2>/dev/null || true
MARSPOT_UPDATE_FEED="http://127.0.0.1:$PORT/feed.json" \
  nohup "$SHELL_BIN" >"$RUN_LOG" 2>&1 < /dev/null &
disown

for _ in $(seq 1 50); do
  grep -q $'\tHELLO_ACK\t' "$SUP_LOG" 2>/dev/null && break
  sleep 0.1
done
grep -q $'\tHELLO_ACK\t' "$SUP_LOG" 2>/dev/null || fail "boot HelloAck never landed"
echo "[2/5] boot OK"

# --- 2. Updater downloads, verifies, stages ---------------------------
# 30 s startup stagger + download; allow 75 s total.  All three live
# binaries ship + stage — marspot-session rides along so silent updates
# carry the per-pane L3 engine in lockstep with core.  (There used to be
# a fourth, marspot-shelld; RFC-003 retired L4 and deleted the target.)
START=$(date +%s)
until [[ -f "$TREE/pending/marspot-core" && -f "$TREE/pending/marspot-shell" \
         && -f "$TREE/pending/marspot-session" ]]; do
  if (( $(date +%s) - START > 75 )); then
    fail "updater never staged all three binaries (download/verify failed?)"
  fi
  sleep 1
done
echo "[3/5] stage OK — updater downloaded, verified signature, staged 3 binaries (incl. marspot-session)"

# --- 3. First trigger: shell self-update ------------------------------
# The trigger starts a Gatekeeper probe of the staged binary before any
# promotion happens.  Unlike the binaries under target/, this one was
# just unpacked from the tarball by the updater, so there is no way to
# pre-warm it — the assessment lands here, and macOS decides how long it
# takes (seconds when idle, minutes when syspolicyd is being flooded —
# 204 s was measured on 2026-07-29).  Running late in a full --real
# suite, after nine other suites have hammered exec, this genuinely
# went over 90 s once; 180 s is the headroom, not a target.
"$SHELL_BIN" --trigger >/dev/null
START=$(date +%s)
until grep -q $'\tSHELL_SELF_UPDATE\t' "$SUP_LOG" 2>/dev/null; do
  if (( $(date +%s) - START > 180 )); then
    fail "no SHELL_SELF_UPDATE within 180 s of the first trigger"
  fi
  sleep 0.5
done
[[ -f "$TREE/current/marspot-shell" ]] || fail "current/marspot-shell missing after self-update"
echo "[4/5] shell self-update OK — exec'd into current/marspot-shell"

# --- 4. Second trigger: core apply -------------------------------------
# Give the new shell a beat to finish reattaching before signalling.
sleep 2
"$SHELL_BIN" --trigger >/dev/null
START=$(date +%s)
# Same story as the shell: the staged core gets probed first.
until grep -q $'\tCORE_SPAWN\t.*binaries/current/marspot-core' "$SUP_LOG" 2>/dev/null; do
  if (( $(date +%s) - START > 180 )); then
    fail "core never spawned from binaries/current within 180 s of the second trigger"
  fi
  sleep 0.5
done
# The freshly-spawned core promotes the staged session engine at boot
# (pending/marspot-session → current/) so L3 panes run it in lockstep.
START=$(date +%s)
until [[ -f "$TREE/current/marspot-session" && ! -f "$TREE/pending/marspot-session" ]]; do
  if (( $(date +%s) - START > 15 )); then
    fail "new core never promoted pending/marspot-session → current/ at boot"
  fi
  sleep 0.5
done
echo "[5/5] core apply OK — new core from current/, session promoted to current/"

cleanup
trap - EXIT
echo "ALL PASS"
