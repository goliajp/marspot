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
# Budget ~90 s.  Builds the release binaries itself, with the
# test-update-key feature; exits 0 on success.
#
# Port 6024 is this project's allocation in the global port
# registry — don't change it without re-registering.

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
# A throwaway signing key for this run.  The binaries are built with
# the `test-update-key` feature, which makes the updater trust the PEM
# that MARSPOT_TEST_UPDATE_PUBKEY names -- only while MARSPOT_STATE_DIR
# names a sandbox, as it does here.  The real release key never has to
# be on the machine that runs the suite.  The build goes to
# target/release, where the sandbox helpers expect it; install-local
# rebuilds without the feature, and a feature build outside a sandbox
# trusts only the real key anyway.
TEST_KEY="$SERVE_DIR/test-update.sec"
openssl ecparam -genkey -name prime256v1 -noout -out "$TEST_KEY" 2>/dev/null \
  || fail "could not generate the test signing key"
openssl ec -in "$TEST_KEY" -pubout -out "$SERVE_DIR/test-update.pub" 2>/dev/null \
  || fail "could not derive the test public key"
export MARSPOT_TEST_UPDATE_PUBKEY="$SERVE_DIR/test-update.pub"
( cd "$ROOT" && cargo build --release --features test-update-key \
    --bin marspot-shell --bin marspot-core --bin marspot-session 2>&1 | tail -2 ) \
  || fail "release build with test-update-key failed"
LC_ALL=C grep -q MARSPOT_TEST_UPDATE_PUBKEY "$ROOT/target/release/marspot-shell" \
  || fail "target/release/marspot-shell was built without test-update-key"
# The release tarball, from exactly those binaries.  Not
# build-release-tarball.sh: it rebuilds without the feature.
make_tarball() {
  local out="$1" stage
  stage=$(mktemp -d "$SERVE_DIR/stage.XXXXXX")
  for b in marspot-shell marspot-core marspot-session; do
    install -m 0755 "$ROOT/target/release/$b" "$stage/$b" || return 1
  done
  # The session engine shipped is another build than the one running, so
  # the running panes must really move: the first character of its build
  # marker's sha is flipped, and the copy re-signed (a changed byte voids
  # the signature, and macOS will not exec it).
  python3 - "$stage/marspot-session" <<'PY' || return 1
import sys
p = sys.argv[1]; b = bytearray(open(p, 'rb').read())
i = b.find(b'MARSPOT_FP=')
while i >= 0 and b.find(b'|END', i, i + 256) < 0:
    i = b.find(b'MARSPOT_FP=', i + 1)
assert i >= 0, 'no build marker'
j = i + len(b'MARSPOT_FP=')
b[j] = ord('1') if b[j] == ord('0') else ord('0')
open(p, 'wb').write(b)
PY
  codesign --force --sign - "$stage/marspot-session" >/dev/null 2>&1 || return 1
  tar -czf "$out" -C "$stage" marspot-shell marspot-core marspot-session
}
make_tarball "$SERVE_DIR/$ASSET" || fail "could not build the release tarball"
openssl dgst -sha256 -sign "$TEST_KEY" -out "$SERVE_DIR/$ASSET.sig" "$SERVE_DIR/$ASSET" \
  || fail "could not sign the release tarball"
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
echo "[1/6] feed OK — signed tarball + feed.json served on :$PORT"

# --- 1. Boot with the fake feed --------------------------------------
dev_kill_shell_core
dev_wipe_state
mkdir -p "$(dirname "$SUP_LOG")"
rm -f "$SUP_LOG" 2>/dev/null || true
MARSPOT_UPDATE_FEED="http://127.0.0.1:$PORT/feed.json" \
  nohup "$SHELL_BIN" >"$RUN_LOG" 2>&1 < /dev/null & dev_record_pid $!
disown

for _ in $(seq 1 50); do
  grep -q $'\tHELLO_ACK\t' "$SUP_LOG" 2>/dev/null && break
  sleep 0.1
done
grep -q $'\tHELLO_ACK\t' "$SUP_LOG" 2>/dev/null || fail "boot HelloAck never landed"
echo "[2/6] boot OK"

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
echo "[3/6] stage OK — updater downloaded, verified signature, staged 3 binaries (incl. marspot-session)"

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
echo "[4/6] shell self-update OK — exec'd into current/marspot-shell"

# --- 4. Second trigger: core apply -------------------------------------
# Give the new shell a beat to finish reattaching before signalling.
sleep 2
L3_BEFORE="$(dev_session_pids | sort | tr '\n' ' ')"
[[ -n "${L3_BEFORE// }" ]] || fail "no pane's L3 is running before the core apply"
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
echo "[5/6] core apply OK — new core from current/, session promoted to current/"

# --- 5. The running panes are on the promoted build, none lost ---------
# The new core asks every reattached L3 not on the promoted build to move
# to it, once, in the way that L3 understands; one already on it is left
# alone.  Every pane that was running must still be (an execv keeps the
# pid), its registry entry must name the promoted build, and it must have
# execv'd at most once.
build_of() { strings "$1" | grep -o 'MARSPOT_FP=[^|]*|[^|]*|END' | head -1 | sed 's/|END$//'; }
want_build=$(build_of "$TREE/current/marspot-session")
[[ -n "$want_build" ]] || fail "current/marspot-session carries no build marker"
START=$(date +%s)
while :; do
  on=0 total=0
  for e in "$MARSPOT_STATE_DIR"/sessions/*/entry.toml; do
    p=$(sed -n 's/^pid = \([0-9][0-9]*\)$/\1/p' "$e")
    case " $L3_BEFORE " in *" $p "*) ;; *) continue ;; esac
    total=$((total+1))
    b=$(sed -n 's/^build = "\(.*\)"$/\1/p' "$e")
    [[ "$b" == "$want_build" ]] && on=$((on+1))
  done
  (( on == total && total > 0 )) && break
  (( $(date +%s) - START > 60 )) && fail "$on/$total panes on the promoted session build within 60 s"
  sleep 0.5
done
for p in $L3_BEFORE; do
  kill -0 "$p" 2>/dev/null || fail "pane L3 pid $p is gone after the update"
  n=$(grep $'\tL3_EXECV_INVOKE\t' "$SUP_LOG" | grep -c $'\t'"$p"$'\t')
  (( n == 1 )) || fail "pane L3 pid $p execv'd $n times for one update"
done
# these L3s are of this build, so they take the frame, not SIGTERM
grep $'\tL3_SWAP_ASK\t' "$SUP_LOG" | grep -q 'how=Frame' \
  || fail "the core never asked a running L3 with a SessionUpdate frame"
echo "[6/6] panes OK — $total L3s moved to the promoted build, each by one execv, none lost"

cleanup
trap - EXIT
echo "ALL PASS"
