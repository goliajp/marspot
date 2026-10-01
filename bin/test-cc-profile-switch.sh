#!/usr/bin/env bash
#
# Switching a claudecode pane's account profile, end to end.
#
# The pieces had tests and the chain did not, and on 2026-10-01 that
# cost three defects in one evening -- all of them green in the unit
# tests, all of them wrong on the machine:
#
#   * the resume line was built as `exec VAR=val claude`, which asks
#     the shell to run a program named `VAR=val`. Every switch started
#     nothing and then waited twenty seconds for it.
#   * the run ended before the new claude had drawn, and the screen it
#     unfroze onto was the one it had cleared itself: black, for as
#     long as the process took to paint.
#   * the resume ran wherever the pane's shell happened to stand, so a
#     session came back bound to another project.
#
# What they have in common is that every assertion about that line read
# the line as text. This runs it.
#
# claude itself is not in the chain: a stand-in records how it was
# called and waits. Driving the real CLI would sign into the user's
# accounts and end their sessions.
#
# Sandbox-only: MARSPOT_STATE_DIR *and* HOME are redirected, so the
# profiles this reads are its own and the user's real ~/.claude* is
# never opened.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

SB=/tmp/marspot-cc-switch
RUNLOG=$SB/run.log
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
CLAUDE_LOG=$SB/claude-invocations.log
MENU_FILE=$SB/badge-menu
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"
UUID=11111111-2222-3333-4444-555555555555

fail() {
  echo "FAIL: $*"
  echo "  --- claude invocations:"; sed 's/^/    /' "$CLAUDE_LOG" 2>/dev/null
  echo "  --- the switch's own steps:"
  grep -E 'profile_cycle' "$APPLOG" 2>/dev/null | tail -20 | cut -c1-200 | sed 's/^/    /'
  exit 1
}
ok() { echo "  ✓ $1"; }

cleanup() {
  dev_kill_shell_core
  # By the pids it recorded, never by name: the stand-in runs as
  # `claude`, and `pkill claude` on this machine would end the user's
  # real sessions.
  if [[ -f "$CLAUDE_LOG" ]]; then
    while read -r pid; do
      [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null
    done < <(sed -n 's/^start pid=\([0-9]*\).*/\1/p' "$CLAUDE_LOG")
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

rm -rf "$SB"; mkdir -p "$SB/bin" "$SB/work" "$SB/elsewhere"

# ── a stand-in for claude ─────────────────────────────────────────
#
# Compiled, not a script: the plugin identifies the agent by argv[0]'s
# basename, and a `#!` script is exec'd as `/bin/sh <path>` -- argv[0]
# would be `sh` and nothing would ever be recognised.
#
# It does three things the real one does and this test depends on:
# records how it was called (including the directory it is running
# in), writes the session record the plugin reads to learn which
# conversation a pane is on, and then waits.
#
# It also moves itself into $SB/work while the pane's shell stays in
# $SB/elsewhere. That divergence is the point: the session lives where
# claude is, and a resume typed into the shell runs where the shell
# is. They were the same directory until the day they were not.
echo "==> building the claude stand-in"
cat > "$SB/claude-stub.c" <<STUB
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
int main(int argc, char **argv) {
    chdir("$SB/work");
    const char *cfg = getenv("CLAUDE_CONFIG_DIR");
    char cwd[4096];
    if (!getcwd(cwd, sizeof cwd)) strcpy(cwd, "-");
    // The record every live claude keeps: which conversation this pid
    // is on. The plugin reads it to bind the pane, and without it
    // there is no session to resume and no switch to test.
    if (cfg) {
        char dir[4096], path[4200];
        snprintf(dir, sizeof dir, "%s/sessions", cfg);
        mkdir(dir, 0755);
        snprintf(path, sizeof path, "%s/%d.json", dir, getpid());
        FILE *s = fopen(path, "w");
        if (s) {
            fprintf(s, "{\"sessionId\":\"$UUID\",\"kind\":\"interactive\"}");
            fclose(s);
        }
    }
    FILE *f = fopen("$CLAUDE_LOG", "a");
    // Nothing to record means this test can observe nothing, and a
    // stand-in sitting there quietly looks exactly like one that was
    // never started -- so say so by dying.
    if (!f) return 9;
    fprintf(f, "start pid=%d CLAUDE_CONFIG_DIR=%s cwd=%s argv=",
            getpid(), cfg ? cfg : "-", cwd);
    for (int i = 1; i < argc; i++) fprintf(f, "%s%s", i > 1 ? " " : "", argv[i]);
    fprintf(f, "\n");
    fclose(f);
    // Take the terminal and draw, the way a TUI does: the switch waits
    // for the program to say it has the terminal (alt screen plus
    // bracketed paste) and then for it to actually paint, because the
    // screen it unfreezes onto is the one it cleared itself. A
    // stand-in that stays silent would be testing the timeouts.
    printf("\033[?1049h\033[?2004h");
    for (int i = 0; i < 64; i++)
        printf("%-63s\n", "stand-in frame");
    fflush(stdout);
    for (;;) pause();
}
STUB
cc -O0 -o "$SB/bin/claude" "$SB/claude-stub.c" || { echo "cc failed"; exit 1; }

# ── this test's own HOME ──────────────────────────────────────────
export HOME="$SB/home"
# The physical path, not the one with `/tmp` in it: on macOS `/tmp` is
# a symlink to `/private/tmp`, and what claude reports as its working
# directory -- which is what the project directory is named after -- is
# the resolved one. Encoding the unresolved path put the transcript in
# a directory nothing would ever look in, and the switch then started a
# fresh session because it could find nothing to resume.
WORK="$(cd "$SB/work" && pwd -P)"
ENCODED="$(printf %s "$WORK" | tr / -)"
for n in 1 2; do
  mkdir -p "$HOME/.claude-profile-$n/projects/$ENCODED" \
           "$HOME/.claude-profile-$n/sessions"
done
# A transcript for the session, so the switch has something to resume.
printf '{"type":"assistant","sessionId":"%s","timestamp":"2026-10-01T00:00:00.000Z"}\n' \
  "$UUID" > "$HOME/.claude-profile-1/projects/$ENCODED/$UUID.jsonl"
export PATH="$SB/bin:$PATH"
: > "$CLAUDE_LOG"

echo "==> launching"
dev_kill_shell_core
rm -rf "$MARSPOT_STATE_DIR"; mkdir -p "$MARSPOT_STATE_DIR/logs"
MARSPOT_SESSION_BIN="$SESSION_BIN" MARSPOT_CORE_BIN="$CORE_BIN" \
  MARSPOT_DEV_BADGE_MENU="$MENU_FILE" \
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null &
disown

wait_for 'L3_SPAWNED' 90 || fail "no pane session was ever spawned"
SID=""
for (( i = 0; i < 150; i++ )); do
  SID="$("$SHELL_BIN" --panes 2>/dev/null | awk 'NR>1 {print $1; exit}')"
  [[ -n "$SID" ]] && break
  sleep 0.4
done
[[ -n "${SID:-}" ]] || fail "no pane to work with ($("$SHELL_BIN" --panes 2>&1 | head -3))"
echo "==> pane $SID"

echo "==> waiting for the pane's shell to read"
READY="$SB/pane-ready"
for (( attempt = 1; attempt <= 10; attempt++ )); do
  "$SHELL_BIN" --send "$SID" "cd $SB/elsewhere && echo reading > $READY" >/dev/null 2>&1 \
    || fail "--send was refused on attempt $attempt"
  for (( i = 0; i < 15; i++ )); do
    [[ -f "$READY" ]] && break 2
    sleep 0.2
  done
done
[[ -f "$READY" ]] || fail "the pane's shell never ran anything typed into it"

echo "==> starting the stand-in in the pane"
sent=0
for (( i = 0; i < 40; i++ )); do
  if "$SHELL_BIN" --send "$SID" "CLAUDE_CONFIG_DIR=$HOME/.claude-profile-1 claude" \
     >/dev/null 2>&1; then sent=1; break; fi
  sleep 0.5
done
(( sent )) || fail "--send claude was refused 40 times"
for (( i = 0; i < 100; i++ )); do
  grep -q '^start ' "$CLAUDE_LOG" && break
  sleep 0.2
done
grep -q '^start ' "$CLAUDE_LOG" || fail "the stand-in did not start"

starts() { grep -c '^start ' "$CLAUDE_LOG"; }
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
LIVE_PID="$(grep '^start ' "$CLAUDE_LOG" | tail -1 | sed -n 's/.*pid=\([0-9]*\).*/\1/p')"
kill -0 "$LIVE_PID" 2>/dev/null || fail "the stand-in exited on its own (pid $LIVE_PID)"
ok "the stand-in is running as pane $SID's agent (pid $LIVE_PID)"

wait_for "claudecode.*sid=$SID|session.bound.*$SID" 60 \
  || fail "the plugin never recognised the pane's agent"
ok "the plugin bound the pane to it"

echo "==> picking profile 2 from the badge menu"
echo "$SID 2" > "$MENU_FILE.tmp" && mv "$MENU_FILE.tmp" "$MENU_FILE"
wait_for 'DEV_BADGE_MENU' 20 || fail "the seam never fired"

# What the switch must do: end the running one, and bring the session
# back under the other profile -- in the directory the session lives
# in, with the conversation's own id.
for (( i = 0; i < 300; i++ )); do
  (( $(starts) > BEFORE )) && break
  sleep 0.2
done
(( $(starts) > BEFORE )) \
  || fail "no claude was started after the pick -- the line the pane was given did not run"
ok "a claude was started by the resume line"
kill -0 "$LIVE_PID" 2>/dev/null && fail "the claude that was running is still running"
ok "the claude that was running was ended"

SECOND="$(grep '^start ' "$CLAUDE_LOG" | tail -1)"
grep -q "CLAUDE_CONFIG_DIR=$HOME/.claude-profile-2" <<<"$SECOND" \
  || fail "the new claude did not get profile 2's config dir: $SECOND"
ok "it came back under profile 2"
grep -q -- "--resume $UUID" <<<"$SECOND" \
  || fail "the new claude was not a resume of this session: $SECOND"
ok "it resumed the conversation rather than starting a new one"
grep -q "cwd=$WORK" <<<"$SECOND" \
  || fail "the resume ran where the shell stood, not where the session lives: $SECOND"
ok "it resumed in the session's own directory, not the shell's"

# The hold over the pane lifts when the run ends, and the run cleared
# the screen itself -- so it has to outlast the first frame.
# The hold lifts when the run ends, so the run has to outlast the
# paint -- and the run is not over until it says so.
for (( i = 0; i < 300; i++ )); do
  grep -qE "profile_cycle\.outcome.*pane $SID" "$APPLOG" && break
  sleep 0.2
done
STEPS="$(awk -F'\t' -v s="pane $SID " '$7 ~ /profile_cycle\.step/ && index($8, s) == 1 {print $8}' "$APPLOG")"
grep -q 'first_frame' <<<"$STEPS" \
  || fail "the run has no step that waits for the new claude to draw: $(tr '\n' ' ' <<<"$STEPS")"
ok "the run has a step that waits for the new claude to draw"

OUTCOME="$(awk -F'\t' -v s="pane $SID " '$7 ~ /profile_cycle\.outcome/ && index($8, s) == 1 {print $8}' "$APPLOG" | tail -1)"
grep -q 'Done' <<<"$OUTCOME" \
  || fail "the switch did not finish cleanly: ${OUTCOME:-<no outcome logged>}"
ok "and it reached the end of the run rather than giving up"

# Order is the whole point: the first frame is waited for before the
# composer comes back, and the run ends after both.
FIRST_FRAME_AT="$(grep -nE "profile_cycle\.step.*pane $SID .*first_frame" "$APPLOG" | tail -1 | cut -d: -f1)"
OUTCOME_AT="$(grep -nE "profile_cycle\.outcome.*pane $SID" "$APPLOG" | tail -1 | cut -d: -f1)"
[[ -n "$FIRST_FRAME_AT" && -n "$OUTCOME_AT" && "$FIRST_FRAME_AT" -lt "$OUTCOME_AT" ]] \
  || fail "the run ended before it waited for the frame (frame@${FIRST_FRAME_AT:-?} outcome@${OUTCOME_AT:-?})"
ok "it waited for the frame before letting the pane go"

echo "PASS — a badge-menu pick moves a live claudecode pane to another profile"
