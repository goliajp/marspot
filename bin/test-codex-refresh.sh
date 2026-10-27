#!/usr/bin/env bash
#
# A pane running an older codex than the one installed is moved onto
# the installed one, end to end: a real pty, a real process the plugin
# recognises as codex, its executable inside an older release
# directory, a SIGTERM, and a `codex resume <thread>` typed back into the
# same shell that starts the current release.
#
# codex itself is not in the chain, as in test-codex-profile-switch.sh:
# a stand-in records how it was started.  With no codex daemon there is
# nothing for peerbus to bind, so the thread each pane runs is handed to
# the plugin through MARSPOT_DEV_CODEX_THREADS.  The stand-in draws no
# input box, so carrying an unsent draft across is not exercised here.
#
# Two windows, one pane each: the pane that has the cursor must be left
# alone, the other refreshed after a minute of quiet.
#
# Sandbox-only: MARSPOT_STATE_DIR and HOME are redirected, so the
# packages this reads are its own and the user's ~/.codex* is never
# opened.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

SB=/tmp/marspot-codex-refresh
RUNLOG=$SB/run.log
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
CODEX_LOG=$SB/codex-invocations.log
THREADS=$SB/threads
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"
OLD=0.1.0-test
NEW=0.2.0-test

fail() {
  echo "FAIL: $*"
  echo "  --- codex invocations:"; sed 's/^/    /' "$CODEX_LOG" 2>/dev/null
  echo "  --- shell lines about the refresh:"
  grep -E 'refresh|PaneFocused' "$APPLOG" 2>/dev/null | tail -25 | cut -c1-220 | sed 's/^/    /'
  exit 1
}
ok() { echo "  ✓ $1"; }

cleanup() {
  dev_kill_shell_core
  # by the pids the stand-ins recorded, never by name: `pkill codex`
  # on this machine would end the user's real sessions
  if [[ -f "$CODEX_LOG" ]]; then
    while read -r pid; do
      [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null
    done < <(sed -n 's/^start pid=\([0-9]*\).*/\1/p' "$CODEX_LOG")
  fi
}
trap cleanup EXIT

wait_for() { # regex, seconds
  local re="$1" secs="$2" i
  for (( i = 0; i < secs * 5; i++ )); do
    grep -qE "$re" "$APPLOG" 2>/dev/null && return 0
    sleep 0.2
  done
  return 1
}

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" || ! -x "$SESSION_BIN" ]]; then
  echo "==> building release binaries"
  ( cd "$ROOT" && cargo build --release \
      --bin marspot-shell --bin marspot-core --bin marspot-session 2>&1 | tail -3 )
fi

# ── a stand-in for codex ──────────────────────────────────────────
#
# Compiled, so argv[0] is `codex` (see test-codex-profile-switch.sh).
# It also records its own executable: which release a pane runs is
# read off that path, and it is the thing this test is about.
echo "==> building the codex stand-in"
rm -rf "$SB"; mkdir -p "$SB/bin"
cat > "$SB/codex-stub.c" <<STUB
#include <libproc.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
int main(int argc, char **argv) {
    FILE *f = fopen("$CODEX_LOG", "a");
    if (!f) return 9;
    char exe[PROC_PIDPATHINFO_MAXSIZE] = "-";
    proc_pidpath(getpid(), exe, sizeof exe);
    const char *home = getenv("CODEX_HOME");
    fprintf(f, "start pid=%d exe=%s CODEX_HOME=%s argv=", getpid(), exe, home ? home : "-");
    for (int i = 1; i < argc; i++) fprintf(f, "%s%s", i > 1 ? " " : "", argv[i]);
    fprintf(f, "\\n");
    fclose(f);
    for (;;) pause();
}
STUB
cc -O0 -o "$SB/codex-stub" "$SB/codex-stub.c" || { echo "cc failed"; exit 1; }

# ── this test's own HOME, with two codex releases installed ───────
export HOME="$SB/home"
PKG="$HOME/.codex/packages/standalone"
for r in "$OLD" "$NEW"; do
  mkdir -p "$PKG/releases/$r/bin"
  cp "$SB/codex-stub" "$PKG/releases/$r/bin/codex"
done
ln -sfn "$PKG/releases/$NEW" "$PKG/current"
# what the installer does: the command on PATH is the current release
ln -sfn "$PKG/current/bin/codex" "$SB/bin/codex"
printf 'model = "gpt-6-astra"\nmodel_reasoning_effort = "medium"\n' > "$HOME/.codex/config.toml"
export PATH="$SB/bin:$PATH"
: > "$CODEX_LOG"
: > "$THREADS"

echo "==> launching with a second window"
dev_kill_shell_core
rm -rf "$MARSPOT_STATE_DIR"; mkdir -p "$MARSPOT_STATE_DIR/logs"
MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  MARSPOT_DEV_EXTRA_WINDOWS=1 MARSPOT_DEV_CODEX_THREADS="$THREADS" \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
disown

wait_for 'L3_SPAWNED' 90 || fail "no pane session was ever spawned"
SIDS=()
for (( i = 0; i < 150; i++ )); do
  SIDS=()
  while read -r sid; do SIDS+=("$sid"); done < <("$SHELL_BIN" --panes 2>/dev/null | awk 'NR>1 {print $1}')
  (( ${#SIDS[@]} >= 2 )) && break
  sleep 0.4
done
(( ${#SIDS[@]} >= 2 )) || fail "expected two panes, got ${#SIDS[@]} ($("$SHELL_BIN" --panes 2>&1 | head -4))"
echo "==> panes ${SIDS[*]}"

# Start the old release in each pane, by its path, once its shell reads.
for sid in "${SIDS[@]}"; do
  ready="$SB/ready-$sid"
  for (( attempt = 1; attempt <= 10; attempt++ )); do
    "$SHELL_BIN" --send "$sid" "echo reading > $ready" >/dev/null 2>&1 \
      || fail "--send to $sid was refused on attempt $attempt"
    for (( i = 0; i < 15; i++ )); do
      [[ -f "$ready" ]] && break 2
      sleep 0.2
    done
  done
  [[ -f "$ready" ]] || fail "pane $sid's shell never ran anything typed into it"
  sent=0
  for (( i = 0; i < 40; i++ )); do
    if "$SHELL_BIN" --send "$sid" "$PKG/releases/$OLD/bin/codex" >/dev/null 2>&1; then sent=1; break; fi
    sleep 0.5
  done
  (( sent )) || fail "starting the old codex in $sid was refused 40 times"
done

starts() { grep -c '^start ' "$CODEX_LOG"; }
for (( i = 0; i < 100; i++ )); do
  (( $(starts) >= 2 )) && break
  sleep 0.2
done
(( $(starts) >= 2 )) || fail "the old stand-ins did not both start"
grep -q "exe=.*/releases/$NEW/" "$CODEX_LOG" && fail "a pane started the new release before any refresh"
ok "both panes run release $OLD"
OLD_PIDS="$(sed -n 's/^start pid=\([0-9]*\).*/\1/p' "$CODEX_LOG" | tr '\n' ' ')"

THREAD_OF() { printf '01a1aaaa-bbbb-7ccc-8ddd-%012d\n' "$1"; }
for sid in "${SIDS[@]}"; do
  echo "$sid $(THREAD_OF "$sid")" >> "$THREADS"
done

# Quiet for a minute is a gate, so this takes at least that long.
echo "==> waiting for the refresh (needs 60 s of quiet)"
for (( i = 0; i < 180; i++ )); do
  grep -q "exe=.*/releases/$NEW/" "$CODEX_LOG" && break
  sleep 1
done
NEWLINE="$(grep "exe=.*/releases/$NEW/" "$CODEX_LOG" | head -1)"
[[ -n "$NEWLINE" ]] || fail "no pane was moved to release $NEW within 180 s"
ok "a pane now runs release $NEW"

REFRESHED="$(grep -oE 'refresh.start.*pane [0-9]+' "$APPLOG" | head -1 | grep -oE '[0-9]+$')"
[[ -n "$REFRESHED" ]] || fail "the log has no refresh.start"
grep -q "argv=resume $(THREAD_OF "$REFRESHED")\$" <<<"$NEWLINE" \
  || fail "the new codex did not resume pane $REFRESHED's thread: $NEWLINE"
ok "it resumed pane $REFRESHED's own thread by id"
grep -q "CODEX_HOME=$HOME/.codex " <<<"$NEWLINE" \
  || fail "the new codex lost its CODEX_HOME: $NEWLINE"
ok "under the same CODEX_HOME"

old_alive=0
for pid in $OLD_PIDS; do kill -0 "$pid" 2>/dev/null && old_alive=$((old_alive + 1)); done
(( old_alive == 1 )) || fail "expected exactly one old codex left running, found $old_alive"
ok "the old codex in that pane was ended; the other pane's is untouched"

OTHER=""
for sid in "${SIDS[@]}"; do [[ "$sid" != "$REFRESHED" ]] && OTHER="$sid"; done
grep -qE "refresh.held.*pane $OTHER:.*held: user_here" "$APPLOG" \
  || fail "pane $OTHER was not held back as the pane with the cursor"
ok "pane $OTHER, which has the cursor, was held back"

# One refresh per pane per installed release: the new process is
# current, so nothing should restart it again.
sleep 10
(( $(grep -c "exe=.*/releases/$NEW/" "$CODEX_LOG") == 1 )) \
  || fail "the refreshed pane was restarted again"
ok "and it was not restarted a second time"

echo "PASS — an outdated codex pane is moved to the installed release, the focused one is not"
