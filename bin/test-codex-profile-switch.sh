#!/usr/bin/env bash
#
# Switching a codex pane's account profile, end to end.
#
# The pieces were covered — the menu is built and read back by unit
# tests, the op runner by its own with a fake host — but the chain had
# never run: a real pty, a real process the plugin recognises as codex,
# a real SIGTERM, and a resume line typed back into the same shell.
# What the handoff said about it was "命令形状正确, 端到端未证".
#
# codex itself is not in the chain.  `CODEX_HOME` and
# `resume --last` are asserted as the arguments the plugin hands over
# — what codex then does with them is codex's, and driving the real
# CLI would sign into the user's accounts and end their sessions.  The
# flag shapes are checked against the installed `codex resume --help`
# separately; this pins OUR half.
#
# Sandbox-only: MARSPOT_STATE_DIR *and* HOME are redirected, so the
# profile directories this reads are its own and the user's real
# ~/.codex* is never opened.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

SB=/tmp/marspot-codex-switch
RUNLOG=$SB/run.log
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
CODEX_LOG=$SB/codex-invocations.log
MENU_FILE=$SB/badge-menu
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"

fail() {
  echo "FAIL: $*"
  echo "  --- codex invocations:"; sed 's/^/    /' "$CODEX_LOG" 2>/dev/null
  echo "  --- last 25 shell lines mentioning codex:"
  grep -i codex "$APPLOG" 2>/dev/null | tail -25 | cut -c1-200 | sed 's/^/    /'
  exit 1
}
ok() { echo "  ✓ $1"; }

cleanup() {
  dev_kill_shell_core
  # By the pids it recorded, never by name: the stand-in runs as
  # `codex`, and `pkill codex` on this machine would end the user's
  # real sessions.
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
# Compiled, not a script: the plugin identifies the agent by argv[0]'s
# basename, and a `#!` script is exec'd as `/bin/sh <path>` — argv[0]
# would be `sh` and nothing would ever be recognised.  It records how
# it was called and then waits, like the real one waiting for input.
echo "==> building the codex stand-in"
rm -rf "$SB"; mkdir -p "$SB/bin"
# The log path is compiled in, not read from the environment: a pane
# does not get the launcher's `MARSPOT_`/`CODEX_`-prefixed variables
# (deliberately — a pane gets the user's environment, never the
# launcher's session), and the first version of this test lost its log
# to a variable under some other name that still never arrived.  A
# literal cannot go missing.
cat > "$SB/codex-stub.c" <<STUB
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
int main(int argc, char **argv) {
    FILE *f = fopen("$CODEX_LOG", "a");
    // Nothing to record means this test can observe nothing, and a
    // stand-in sitting there quietly looks exactly like one that was
    // never started — so say so by dying.
    if (!f) return 9;
    const char *home = getenv("CODEX_HOME");
    fprintf(f, "start pid=%d CODEX_HOME=%s argv=", getpid(), home ? home : "-");
    for (int i = 1; i < argc; i++) fprintf(f, "%s%s", i > 1 ? " " : "", argv[i]);
    fprintf(f, "\\n");
    fclose(f);
    for (;;) pause();
}
STUB
cc -O0 -o "$SB/bin/codex" "$SB/codex-stub.c" || { echo "cc failed"; exit 1; }

# ── this test's own HOME ──────────────────────────────────────────
#
# `~/.codex-profile-N` is where the profiles live and `~/.codex` is the
# symlink that says which one is current — the plugin reads both off
# $HOME, so the only way to test it without touching the user's
# accounts is to give it another $HOME.
export HOME="$SB/home"
mkdir -p "$HOME/.codex-profile-1" "$HOME/.codex-profile-2"
ln -sfn "$HOME/.codex-profile-1" "$HOME/.codex"
for n in 1 2; do
  printf 'model = "gpt-6-astra"\nmodel_reasoning_effort = "medium"\n' \
    > "$HOME/.codex-profile-$n/config.toml"
done
export PATH="$SB/bin:$PATH"
: > "$CODEX_LOG"

echo "==> launching"
dev_kill_shell_core
rm -rf "$MARSPOT_STATE_DIR"; mkdir -p "$MARSPOT_STATE_DIR/logs"
MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  MARSPOT_DEV_BADGE_MENU="$MENU_FILE" \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null &
disown

# A cold first exec of each binary can take seconds on a host whose
# Gatekeeper daemon is busy, and the supervisor gives a core that
# misses its HelloAck a second try — so this waits for a pane to
# actually be addressable rather than for a fixed time after boot.
wait_for 'L3_SPAWNED' 90 || fail "no pane session was ever spawned"
SID=""
for (( i = 0; i < 150; i++ )); do
  SID="$("$SHELL_BIN" --panes 2>/dev/null | awk 'NR>1 {print $1; exit}')"
  [[ -n "$SID" ]] && break
  sleep 0.4
done
[[ -n "${SID:-}" ]] || fail "no pane to work with ($("$SHELL_BIN" --panes 2>&1 | head -3))"
echo "==> pane $SID"

# A pane can be addressable a moment before its shell is reading, and
# a paste that lands in that gap is swallowed.  So the shell says when
# it is reading, and only then is `codex` typed — once.
#
# Retrying `codex` itself is what the previous version did, and it
# poisoned the measurement: the extra line was not lost, it sat in the
# pty until the first codex was killed, and then ran as a bare `codex`
# that the switch got the blame for.  The probe is safe to repeat
# because every copy writes the same file.
echo "==> waiting for the pane's shell to read"
READY="$SB/pane-ready"
for (( attempt = 1; attempt <= 10; attempt++ )); do
  "$SHELL_BIN" --send "$SID" "echo reading > $READY" >/dev/null 2>&1 \
    || fail "--send was refused on attempt $attempt"
  for (( i = 0; i < 15; i++ )); do
    [[ -f "$READY" ]] && break 2
    sleep 0.2
  done
done
[[ -f "$READY" ]] || fail "the pane's shell never ran anything typed into it"

echo "==> starting the stand-in in the pane"
# Retried only while it is REFUSED — a refusal queues nothing, so this
# cannot leave a second `codex` waiting in the pty the way retrying a
# successful send did.  The refusal to expect is "pane is busy": the
# probe above is still an op for a moment after its file appears.
sent=0
for (( i = 0; i < 40; i++ )); do
  if "$SHELL_BIN" --send "$SID" codex >/dev/null 2>&1; then sent=1; break; fi
  sleep 0.5
done
(( sent )) || fail "--send codex was refused 40 times ($("$SHELL_BIN" --send "$SID" codex 2>&1))"
for (( i = 0; i < 100; i++ )); do
  grep -q '^start ' "$CODEX_LOG" && break
  sleep 0.2
done
grep -q '^start ' "$CODEX_LOG" || fail "the stand-in did not start"

starts() { grep -c '^start ' "$CODEX_LOG"; }
settle() { # no new start for 2 s
  local last=-1 n
  while :; do
    n="$(starts)"
    [[ "$n" == "$last" ]] && return 0
    last="$n"; sleep 2
  done
}
settle
BEFORE="$(starts)"
LIVE_PID="$(grep '^start ' "$CODEX_LOG" | tail -1 | sed -n 's/.*pid=\([0-9]*\).*/\1/p')"
kill -0 "$LIVE_PID" 2>/dev/null || fail "the stand-in exited on its own (pid $LIVE_PID)"
ok "the stand-in is running as pane $SID's agent (pid $LIVE_PID)"

# The plugin has to SEE it before a menu pick can mean anything: the
# badge is published off its own scan, and a pick on a pane it has not
# bound is dropped.
wait_for "codex.*sid=$SID" 30 \
  || fail "the plugin never recognised the pane's agent"
ok "the plugin bound the pane to it"

echo "==> picking profile 2 from the badge menu"
# Renamed into place, so the tick that reads it cannot catch it
# half-written — a plain `>` redirect once lost the pick that way.
echo "$SID 2" > "$MENU_FILE.tmp" && mv "$MENU_FILE.tmp" "$MENU_FILE"
wait_for 'DEV_BADGE_MENU' 20 || fail "the seam never fired"

# What the switch must do: end the running one, and bring the session
# back under the other profile's CODEX_HOME.
for (( i = 0; i < 150; i++ )); do
  (( $(starts) > BEFORE )) && break
  sleep 0.2
done
(( $(starts) > BEFORE )) || fail "no codex was started after the pick"
kill -0 "$LIVE_PID" 2>/dev/null && fail "the codex that was running is still running"
ok "the codex that was running was ended"

SECOND="$(grep '^start ' "$CODEX_LOG" | tail -1)"
grep -q "CODEX_HOME=$HOME/.codex-profile-2" <<<"$SECOND" \
  || fail "the new codex did not get profile 2's home: $SECOND"
ok "it came back under profile 2's CODEX_HOME"
grep -q 'argv=resume --last' <<<"$SECOND" \
  || fail "the new codex was not a resume: $SECOND"
ok "it resumed the session rather than starting a new one"

echo "PASS — a badge-menu pick moves a live codex pane to another profile"
