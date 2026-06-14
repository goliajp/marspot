#!/usr/bin/env bash
#
# End-to-end sandbox dry-run of `install-shelld.sh --apply-pending-execv`
# — the path `install-local.sh --with-shelld` now drives by default.
#
# Walks the production install logic against a throwaway launchctl
# label + a real marspot-shelld binary in a sandboxed state dir, then
# asserts (a) shelld PID stable across the swap (= execv preserved
# identity, sessions were not SIGHUP'd), (b) the structured marspot.log
# contains both the SHELLD_UPDATE_APPLY_EXECV event (from the install
# script) and the EXECV_RESUME_BEGIN event (from the new image's main
# function), and (c) the post-swap session-table size matches the
# pre-swap one. No real GUI involved.
#
# Mirrors test-shelld-probation.sh's launchctl-throwaway pattern but
# drives the in-place execv path instead of the bootout/bootstrap
# rollback path that test covers.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UID_NUM="$(id -u)"
INSTALL="$ROOT/bin/install-shelld.sh"
PROFILE="${MARSPOT_TEST_PROFILE:-debug}"
SHELLD="$ROOT/target/$PROFILE/marspot-shelld"
SESSION_PROBE="$ROOT/target/$PROFILE/examples/shelld_session_probe"
[[ -x "$SHELLD" ]] || { echo "FAIL: build first — cargo build"; exit 1; }
[[ -x "$SESSION_PROBE" ]] \
  || { echo "FAIL: build first — cargo build -p marspot-session --example shelld_session_probe"; exit 1; }

LABEL="com.marspot.shelld.execvtest.$$"
STATE_DIR="/tmp/marspot-install-execv.$$"
PLIST="$STATE_DIR/test.plist"
BIN="$STATE_DIR/fake-bundle/marspot-shelld"
TREE="$STATE_DIR/binaries"
SUP_LOG="$STATE_DIR/logs/marspot.log"

# Probation 10 s rather than the production 30 s so the test runs fast.
export MARSPOT_STATE_DIR="$STATE_DIR"
export MARSPOT_SHELLD_LABEL="$LABEL"
export MARSPOT_SHELLD_PLIST="$PLIST"
export MARSPOT_SHELLD_BIN="$BIN"
export MARSPOT_SHELLD_PROBATION_S=10

fail() {
  echo "FAIL: $*"
  echo "  (marspot.log execv slice):"
  grep -E $'\t(EXECV_|SHELLD_)' "$SUP_LOG" 2>/dev/null | tail -10 | sed 's/^/    /'
  echo "  (launchctl):"
  launchctl print "gui/$UID_NUM/$LABEL" 2>&1 | grep -E 'state|pid' | head -4 | sed 's/^/    /'
  exit 1
}

cleanup() {
  launchctl bootout "gui/$UID_NUM/$LABEL" >/dev/null 2>&1 || true
  rm -rf "$STATE_DIR"
}
trap cleanup EXIT

# Sanity-guard: refuse to run if the label resolves to anything other
# than the throwaway pattern. Belt-and-braces against env override mishaps.
[[ "$LABEL" == com.marspot.shelld.execvtest.* ]] \
  || fail "label '$LABEL' is not a throwaway test label"

mkdir -p "$STATE_DIR/logs" "$(dirname "$BIN")" "$TREE/current"
# Real marspot-shelld at both the "bundle" path (launchctl ProgramArguments)
# and the binaries/current/ slot (what the daemon's own execv path points
# at on a successful promote). Pre-strip provenance xattrs to dodge the
# 30-60 s _dyld_start gatekeeper stall on first launch.
cp "$SHELLD" "$BIN"
cp "$SHELLD" "$TREE/current/marspot-shelld"
chmod +x "$BIN" "$TREE/current/marspot-shelld"
xattr -c "$BIN" "$TREE/current/marspot-shelld" 2>/dev/null || true

# LaunchAgent plist for the throwaway label, ProgramArguments pointed at
# our BIN. The MARSPOT_STATE_DIR env var must survive into the daemon
# (otherwise it'd write into the production state dir) — pass it via
# EnvironmentVariables in the plist.
cat > "$PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key><array><string>$BIN</string></array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>MARSPOT_STATE_DIR</key><string>$STATE_DIR</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$STATE_DIR/logs/shelld.log</string>
  <key>StandardErrorPath</key><string>$STATE_DIR/logs/shelld.err</string>
  <key>ThrottleInterval</key><integer>5</integer>
</dict>
</plist>
PLIST

launchctl bootstrap "gui/$UID_NUM" "$PLIST" || fail "launchctl bootstrap failed"

# Wait for shelld to come up + bind socket.
SOCK="$STATE_DIR/shelld.sock"
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  [[ -S "$SOCK" ]] && break
  sleep 0.2
done
[[ -S "$SOCK" ]] || fail "shelld did not bind socket"
PRE_PID="$(launchctl print "gui/$UID_NUM/$LABEL" 2>/dev/null \
  | awk -F'=' '/^\tpid =/{gsub(/[ \t]/,"",$2); print $2; exit}')"
PRE_INODE="$(stat -f '%i' "$SOCK")"
echo "shelld up: pid=$PRE_PID, inode=$PRE_INODE"

# Provision 3 sessions through the real client API; the install-shelld
# execv path has to preserve every one.
"$SESSION_PROBE" create 3 > "$STATE_DIR/baseline.tsv" 2>/dev/null \
  || fail "session_probe create failed"
PRE_N="$(wc -l <"$STATE_DIR/baseline.tsv" | tr -d ' ')"
[[ "$PRE_N" == "3" ]] || fail "expected 3 sessions, got $PRE_N"
echo "baseline: 3 sessions"

# Stage a byte-identical pending so we exercise the swap mechanics
# without changing daemon behaviour — the structured log alone
# tells us the swap actually happened.
mkdir -p "$TREE/pending"
cp "$SHELLD" "$TREE/pending/marspot-shelld"
chmod +x "$TREE/pending/marspot-shelld"
xattr -c "$TREE/pending/marspot-shelld" 2>/dev/null || true

# Drive install-shelld.sh --apply-pending-execv. The script will:
#   1. Update the bundle binary (cp pending → $BIN).
#   2. Look up shelld's pid via launchctl print + awk.
#   3. kill -USR1 the daemon.
#   4. Poll the pid for $MARSPOT_SHELLD_PROBATION_S seconds.
#   5. Decide PASS (same-pid throughout) / FAIL (pid changed = launchd
#      respawn = sessions lost).
"$INSTALL" --apply-pending-execv \
  || fail "install-shelld --apply-pending-execv exited non-zero"

POST_PID="$(launchctl print "gui/$UID_NUM/$LABEL" 2>/dev/null \
  | awk -F'=' '/^\tpid =/{gsub(/[ \t]/,"",$2); print $2; exit}')"
POST_INODE="$(stat -f '%i' "$SOCK")"
echo "post-execv: pid=$POST_PID, inode=$POST_INODE"

[[ "$POST_PID" == "$PRE_PID" ]] \
  || fail "shelld pid drifted $PRE_PID → $POST_PID (launchd respawned = sessions lost)"
[[ "$POST_INODE" == "$PRE_INODE" ]] \
  || fail "socket inode drifted $PRE_INODE → $POST_INODE (listen fd not inherited)"

# Structured log: install-shelld.sh sup_log() wrote SHELLD_UPDATE_APPLY_EXECV
# at swap time; the new shelld image's main() wrote EXECV_RESUME_BEGIN;
# the install probation success path wrote SHELLD_UPDATE_STABLE.
grep -q $'\tSHELLD_UPDATE_APPLY_EXECV\t' "$SUP_LOG" 2>/dev/null \
  || fail "SHELLD_UPDATE_APPLY_EXECV event missing from marspot.log"
grep -q $'\tEXECV_RESUME_BEGIN\t' "$SUP_LOG" 2>/dev/null \
  || fail "EXECV_RESUME_BEGIN event (from new image) missing from marspot.log"
grep -q $'\tSHELLD_UPDATE_STABLE\t' "$SUP_LOG" 2>/dev/null \
  || fail "SHELLD_UPDATE_STABLE event missing from marspot.log"

# All 3 sessions still alive + same child PIDs after the swap.
"$SESSION_PROBE" list > "$STATE_DIR/post.tsv" 2>/dev/null \
  || fail "session_probe list failed post-swap"
POST_N="$(wc -l <"$STATE_DIR/post.tsv" | tr -d ' ')"
[[ "$POST_N" == "3" ]] || fail "expected 3 sessions post-swap, got $POST_N"
awk -F'\t' '{print $1"\t"$2}' "$STATE_DIR/post.tsv" | sort > "$STATE_DIR/post.norm"
sort "$STATE_DIR/baseline.tsv" > "$STATE_DIR/base.norm"
diff -q "$STATE_DIR/base.norm" "$STATE_DIR/post.norm" >/dev/null \
  || fail "session id ↔ child pid drift across swap"
while IFS=$'\t' read -r sid spid alive; do
  [[ "$alive" == "true" ]] || fail "session $sid not alive after swap (child=$spid)"
  kill -0 "$spid" 2>/dev/null || fail "child PID $spid (session $sid) dead in OS"
done < "$STATE_DIR/post.tsv"

echo
echo "ALL PASSED: install-shelld --apply-pending-execv preserved pid + listen fd + 3 sessions"
