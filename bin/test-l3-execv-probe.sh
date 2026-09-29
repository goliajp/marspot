#!/usr/bin/env bash
#
# L3 execv self-update — the pre-execv probe.
#
# An L3 that gets SIGTERM'd because `current/marspot-session` differs
# from its own image used to prepare the fd handoff and `execv` straight
# away.  macOS charges the Gatekeeper assessment on a new binary's first
# exec and that charge has no upper bound: on 2026-07-29 twenty L3s sat
# inside it for 114 s each, every one with a frozen pane, because they
# had already stopped serving in order to hand over.
#
# Now the candidate is exec'd once on a background thread first, and the
# loop keeps driving the PTY until the verdict arrives.  This covers the
# rejection arm, which is the one carrying a user-visible guarantee: a
# candidate that cannot start must leave the pane alive on the image it
# already has — the same call L1 and L2 make.
#
# The staged candidate is a shell script carrying a MARSPOT_FP marker.
# `read_binary_fingerprint` only scans the file for `MARSPOT_FP=…|END`,
# so this reads as "a different build" (which is what arms the execv)
# while being something that cannot actually run (which is what the
# probe exists to catch).
#
# Sandbox-only; never touches the installed app.  Run after
# `cargo build --release`.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
SHELL_BIN="$ROOT/target/release/marspot-shell"
SESSION_BIN="$ROOT/target/release/marspot-session"
SUP_LOG="$MARSPOT_STATE_DIR/logs/marspot.log"
TREE="$MARSPOT_STATE_DIR/binaries"
RUN_LOG=/tmp/marspot-test-l3-execv-probe.log

fail() {
  echo "FAIL: $*"
  echo "  (last 25 L3 lines):"
  grep -E "session|l3\." "$SUP_LOG" 2>/dev/null | tail -25 | sed 's/^/    /'
  exit 1
}
cleanup() { dev_kill_shell_core; }
trap cleanup EXIT

if [[ ! -x "$SHELL_BIN" || ! -x "$SESSION_BIN" ]]; then
  echo "==> building release binaries"
  ( cd "$ROOT" && cargo build --release \
      --bin marspot-shell --bin marspot-core --bin marspot-session 2>&1 | tail -3 )
fi

cleanup
dev_ensure_shelld || fail "sandbox shelld"
dev_wipe_state
mkdir -p "$(dirname "$SUP_LOG")"
rm -f "$SUP_LOG" 2>/dev/null || true; : > "$RUN_LOG"

# --- 1. Boot until at least one L3 is driving a PTY ------------------
nohup "$SHELL_BIN" >"$RUN_LOG" 2>&1 < /dev/null &
disown
for _ in $(seq 1 100); do
  grep -q $'\tL3_SIGTERM_INSTALLED\t' "$SUP_LOG" 2>/dev/null && break
  sleep 0.1
done
grep -q $'\tL3_SIGTERM_INSTALLED\t' "$SUP_LOG" 2>/dev/null \
  || fail "no L3 came up within 10 s"

# macOS ships bash 3.2 — no mapfile.
L3_COUNT=$(pgrep -f "$SESSION_BIN( |$)" 2>/dev/null | wc -l | tr -d ' ')
TARGET_PID=$(pgrep -f "$SESSION_BIN( |$)" 2>/dev/null | head -1)
[[ -n "$TARGET_PID" ]] || fail "no live marspot-session processes after boot"
echo "[1/3] boot OK — $L3_COUNT L3s up, probing against pid=$TARGET_PID"

# --- 2. Stage an unrunnable candidate with a foreign fingerprint -----
mkdir -p "$TREE/current"
cat >"$TREE/current/marspot-session" <<'BADSESSION'
#!/bin/bash
# MARSPOT_FP=0000000000000000000000000000000000000000|0|END
exit 1
BADSESSION
chmod 0755 "$TREE/current/marspot-session"

kill -TERM "$TARGET_PID" 2>/dev/null || fail "could not SIGTERM L3 pid=$TARGET_PID"

START=$(date +%s)
until grep -q $'\tl3.execv.probe_failed\t' "$SUP_LOG" 2>/dev/null; do
  if (( $(date +%s) - START > 60 )); then
    fail "L3 never reported a failed probe within 60 s"
  fi
  sleep 0.3
done
grep -q $'\tL3_EXECV_PROBE_START\t' "$SUP_LOG" 2>/dev/null \
  || fail "probe never started — SIGTERM went straight to the old execv path"
echo "[2/3] probe OK — L3_EXECV_PROBE_START then probe_failed"

# --- 3. The pane must have survived ---------------------------------
# The whole point: a candidate that cannot start costs nothing.  The
# same pid must still be there, and it must not have handed anything
# over.
sleep 1
if ! kill -0 "$TARGET_PID" 2>/dev/null; then
  fail "L3 pid=$TARGET_PID exited — a failed probe must leave the pane serving"
fi
if grep -q $'\tL3_EXECV_INVOKE\t' "$SUP_LOG" 2>/dev/null; then
  fail "L3 exec'd into a candidate that failed its probe"
fi
echo "[3/3] containment OK — pid=$TARGET_PID still serving, no execv attempted"

cleanup
trap - EXIT
echo "ALL PASS — a staged L3 that cannot start never costs the running pane"
