#!/usr/bin/env bash
#
# NEGATIVE silent-update test — the adversarial complement to
# test-real-update.sh.  The entire trust model rests on
# `updater::verify_signature` (embedded P-256 pubkey, openssl dgst
# -verify): if a tampered / wrong-key / unsigned release could ever get
# staged, a compromised GitHub account or CDN owns every user's daily
# terminal.  test-real-update covers the HAPPY path only; this drives
# the REAL `check_and_stage` network path against three malicious feeds
# and asserts the updater REJECTS each one — never staging, never
# touching `binaries/current/`.
#
# Each case is its own boot: the updater polls its feed once ~30 s
# after launch (the production stagger), logs `[updater] check failed`
# on rejection, then sleeps.  We wait for that positive rejection
# signal — not merely the absence of staging — then assert:
#
#   1. `[updater] check failed` appeared in the shell run log
#   2. `binaries/pending/` holds none of the three binaries
#   3. `binaries/current/` is byte-for-byte unchanged across the poll
#   4. the supervisor log shows no swap / self-update event
#
# Three boots × (~30 s stagger + verify) → budget ~150 s.  Run after
# `cargo build --release`.  Exits 0 on success.
#
# Port 6025 is this project's second registry allocation (the first,
# 6024, belongs to test-real-update.sh — keeping them distinct lets the
# two run back-to-back without socket contention).

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
SHELL_BIN="$ROOT/target/release/marspot-shell"
SUP_LOG="$MARSPOT_STATE_DIR/logs/supervisor.log"
TREE="$MARSPOT_STATE_DIR/binaries"
RUN_LOG=/tmp/marspot-test-negative-update.log
PORT=6025
ASSET=marspot-aarch64-apple-darwin.tar.gz
SERVE_DIR=$(mktemp -d /tmp/marspot-negfeed.XXXXXX)
HTTP_PID=

fail() {
  echo "FAIL: $*"
  echo "  (last 20 supervisor events):"
  tail -20 "$SUP_LOG" 2>/dev/null | sed 's/^/    /'
  echo "  (shell run log tail):"
  tail -15 "$RUN_LOG" 2>/dev/null | sed 's/^/    /'
  exit 1
}

cleanup() {
  [[ -n "$HTTP_PID" ]] && kill "$HTTP_PID" >/dev/null 2>&1
  dev_kill_shell_core
  rm -rf "$SERVE_DIR"
}
trap cleanup EXIT

# --- 0. Build ONE pristine (unsigned) tarball + a throwaway wrong key --
[[ -f "$ROOT/keys/marspot-update.sec" ]] \
  || fail "no keys/marspot-update.sec — generate it before running (see docs/silent-update.md)"
PROD_KEY="$ROOT/keys/marspot-update.sec"
WRONG_KEY="$SERVE_DIR/wrong.sec"
openssl ecparam -genkey -name prime256v1 -noout -out "$WRONG_KEY" 2>/dev/null \
  || fail "could not generate throwaway wrong key"

BASE="$SERVE_DIR/base.tar.gz"
"$ROOT/bin/build-release-tarball.sh" --output "$BASE" >/dev/null \
  || fail "build-release-tarball.sh failed"
[[ -f "$BASE" ]] || fail "base tarball missing after build"

# Helper: write a releases/latest-shaped feed.  $1=case dir (relative),
# $2=1 to include the .sig asset, 0 to omit it.  Asset order matters —
# tarball entry first (the scraper takes the first match of the name,
# and "<asset>" is a substring of "<asset>.sig").
write_feed() {
  local dir="$1" with_sig="$2"
  if (( with_sig )); then
    cat > "$SERVE_DIR/$dir/feed.json" <<EOF
{
  "tag_name": "v99.99.99",
  "assets": [
    { "name": "$ASSET",     "browser_download_url": "http://127.0.0.1:$PORT/$dir/$ASSET" },
    { "name": "$ASSET.sig", "browser_download_url": "http://127.0.0.1:$PORT/$dir/$ASSET.sig" }
  ]
}
EOF
  else
    cat > "$SERVE_DIR/$dir/feed.json" <<EOF
{
  "tag_name": "v99.99.99",
  "assets": [
    { "name": "$ASSET", "browser_download_url": "http://127.0.0.1:$PORT/$dir/$ASSET" }
  ]
}
EOF
  fi
}

# --- Case "wrongkey": valid signature, but made with a key the shipped
#     updater's embedded pubkey can't verify. -------------------------
mkdir -p "$SERVE_DIR/wrongkey"
cp "$BASE" "$SERVE_DIR/wrongkey/$ASSET"
openssl dgst -sha256 -sign "$WRONG_KEY" -out "$SERVE_DIR/wrongkey/$ASSET.sig" \
  "$SERVE_DIR/wrongkey/$ASSET" 2>/dev/null \
  || fail "could not sign wrongkey tarball"
write_feed wrongkey 1

# --- Case "tampered": signed with the REAL key over pristine bytes,
#     then the tarball is mutated after signing → digest mismatch. ----
mkdir -p "$SERVE_DIR/tampered"
cp "$BASE" "$SERVE_DIR/tampered/$ASSET"
openssl dgst -sha256 -sign "$PROD_KEY" -out "$SERVE_DIR/tampered/$ASSET.sig" \
  "$SERVE_DIR/tampered/$ASSET" 2>/dev/null \
  || fail "could not sign tampered tarball"
# Corrupt AFTER signing: the .sig is valid for the original bytes only.
printf 'evil-trailing-bytes' >> "$SERVE_DIR/tampered/$ASSET"
write_feed tampered 1

# --- Case "nosig": the feed lists the tarball but no .sig asset at all.
#     Unsigned releases must be rejected outright. -------------------
mkdir -p "$SERVE_DIR/nosig"
cp "$BASE" "$SERVE_DIR/nosig/$ASSET"
write_feed nosig 0

python3 -m http.server "$PORT" --bind 127.0.0.1 -d "$SERVE_DIR" >/dev/null 2>&1 &
HTTP_PID=$!
disown
sleep 0.5
curl -sf "http://127.0.0.1:$PORT/wrongkey/feed.json" >/dev/null \
  || fail "feed server didn't come up on :$PORT (squatter? check port registry)"
echo "[setup] OK — 3 malicious feeds served on :$PORT (wrongkey / tampered / nosig)"

# --- Per-case driver -------------------------------------------------
# $1 = case dir / label.  Boots a fresh shell pointed at the case feed,
# waits for the updater's rejection, asserts nothing was staged and
# current/ is untouched.
run_case() {
  local label="$1" reason="$2"
  echo "--- case: $label ---"
  dev_ensure_shelld || fail "[$label] sandbox shelld"
  dev_kill_shell_core
  dev_wipe_state
  mkdir -p "$(dirname "$SUP_LOG")"
  : > "$SUP_LOG" 2>/dev/null || true
  : > "$RUN_LOG"

  MARSPOT_UPDATE_FEED="http://127.0.0.1:$PORT/$label/feed.json" \
    nohup "$SHELL_BIN" >"$RUN_LOG" 2>&1 < /dev/null &
  disown

  local i
  for i in $(seq 1 50); do
    grep -q HELLO_ACK "$SUP_LOG" 2>/dev/null && break
    sleep 0.1
  done
  grep -q HELLO_ACK "$SUP_LOG" 2>/dev/null || fail "[$label] boot HelloAck never landed"

  # Snapshot current/ once the bootstrap has settled, BEFORE the poll
  # fires (~30 s stagger gives ample margin).
  local cur_before
  cur_before=$(cd "$TREE" 2>/dev/null && find current -type f -exec shasum {} + 2>/dev/null | sort)

  # Wait for the positive rejection signal — AND require it carry the
  # expected cause, so the test can't pass on an unrelated error (e.g. a
  # network hiccup) while the signature gate is silently broken.  The
  # updater stagger is ~30 s; allow 60 s for stagger + download + verify.
  local START; START=$(date +%s)
  until grep -qE "\[updater\] check failed.*$reason" "$RUN_LOG" 2>/dev/null; do
    if (( $(date +%s) - START > 60 )); then
      grep -q "\[updater\] check failed" "$RUN_LOG" 2>/dev/null \
        && fail "[$label] updater rejected but for the WRONG reason (expected /$reason/)"
      fail "[$label] updater never logged a rejection — did it stage a bad release?"
    fi
    sleep 1
  done

  # Assert 2: nothing staged.
  for b in marspot-core marspot-shell marspot-shelld; do
    [[ -e "$TREE/pending/$b" ]] \
      && fail "[$label] updater STAGED $b from a rejected release — trust gate breached"
  done

  # Assert 3: current/ byte-for-byte unchanged.
  local cur_after
  cur_after=$(cd "$TREE" 2>/dev/null && find current -type f -exec shasum {} + 2>/dev/null | sort)
  [[ "$cur_before" == "$cur_after" ]] \
    || fail "[$label] binaries/current changed across a rejected poll"

  # Assert 4: no swap / self-update event.
  grep -qE "UPDATE_SWAP|SHELL_SELF_UPDATE|UPDATE_STABLE" "$SUP_LOG" 2>/dev/null \
    && fail "[$label] supervisor applied an update from a rejected release"

  echo "    OK — rejected, nothing staged, current/ untouched"
  dev_kill_shell_core
}

run_case wrongkey "signature verification failed"
run_case tampered "signature verification failed"
run_case nosig    "unsigned releases are rejected"

cleanup
trap - EXIT
echo "ALL PASS — all three malicious releases rejected; trust gate holds"
