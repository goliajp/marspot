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
# codex itself is not in the chain, and neither is its app-server
# daemon: with no daemon to name the pane's thread, the switch has only
# the terminal to go by.  It holds a pick while the terminal is still
# drawing, and once the pane has been quiet it hands the conversation
# off to a codex started under the other profile's `CODEX_HOME`.  The
# stand-in writes a rollout in the shape codex does, so the handoff has
# a conversation to carry, and the test reads back what the new codex
# was typed and the document it names.  The
# resume-by-id path needs the daemon and is pinned by unit tests
# against a stand-in daemon; driving the real CLI would sign into the
# user's accounts and end their sessions.
#
# Sandbox-only: MARSPOT_STATE_DIR *and* HOME are redirected, so the
# profile directories this reads are its own and the user's real
# ~/.codex* is never opened.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

# A scratch directory of this run's own, removed when the run passes:
# the fixed name was shared by every run of the suite on the machine.
SB="$(dev_tmp marspot-codex-switch)"
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
  local rc=$?
  dev_kill_shell_core
  # By the pids it recorded, never by name: the stand-in runs as
  # `codex`, and `pkill codex` on this machine would end the user's
  # real sessions.
  if [[ -f "$CODEX_LOG" ]]; then
    while read -r pid; do
      [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null
    done < <(sed -n 's/^start pid=\([0-9]*\).*/\1/p' "$CODEX_LOG")
  fi
  (( rc != 0 )) || rm -rf "$SB"
}
trap cleanup EXIT

# Lines the app log holds now.  Every wait takes one of these first and
# only accepts lines written after it: a match anywhere in the log let
# an earlier pass of the same event satisfy a later wait.
mark() { if [[ -f "$APPLOG" ]]; then wc -l < "$APPLOG" | tr -d ' '; else echo 0; fi; }
after() { tail -n "+$(( $1 + 1 ))" "$APPLOG" 2>/dev/null; }

wait_for() { # regex, seconds, baseline from `mark`
  local re="$1" secs="$2" base="$3" i
  for (( i = 0; i < secs * 5; i++ )); do
    after "$base" | grep -qE "$re" && return 0
    sleep 0.2
  done
  return 1
}
# a log field `sid=N`, and not the start of `sid=N3`
sid_is() { printf 'sid=%s([[:space:]]|$)' "$1"; }

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
# it was called, writes a rollout of one finished turn the way codex
# does, draws codex's input box and then logs every line typed at it.
# The turn's text carries the stand-in's pid, so the handoff document
# can only hold it if it was read from this run's rollout.
echo "==> building the codex stand-in"
rm -rf "$SB"; mkdir -p "$SB/bin"
# The log path is compiled in, not read from the environment: a pane
# does not get the launcher's `MARSPOT_`/`CODEX_`-prefixed variables
# (deliberately — a pane gets the user's environment, never the
# launcher's session), and the first version of this test lost its log
# to a variable under some other name that still never arrived.  A
# literal cannot go missing.  The default home is compiled in for the
# same reason: with no `CODEX_HOME` the plugin looks under the shell's
# own `$HOME/.codex`, which is this test's, whatever the pane inherits.
cat > "$SB/codex-stub.c" <<STUB
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/stat.h>
static void mkdirs(char *p) {
    for (char *s = p + 1; *s; s++)
        if (*s == '/') { *s = 0; mkdir(p, 0755); *s = '/'; }
    mkdir(p, 0755);
}
// one item_completed event, the record the handoff reads a turn from
static void item(FILE *r, const char *type, const char *text, const char *phase) {
    fprintf(r, "{\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":"
               "{\"type\":\"%s\",\"content\":[{\"type\":\"text\",\"text\":\"%s\"}]%s%s%s}}}\\n",
            type, text, phase ? ",\"phase\":\"" : "", phase ? phase : "", phase ? "\"" : "");
}
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

    // a CODEX_HOME inherited from outside the sandbox could be a real
    // codex home: never write a rollout there
    if (home && strncmp(home, "$SB/", strlen("$SB/")) != 0) return 10;

    // sessions/YYYY/MM/DD/rollout-<time>-<uuid>.jsonl, opened by a
    // session_meta naming the directory it runs in
    char cwd[1024], dir[1400], path[1600], day[16], stamp[32], id[40], text[64];
    if (!getcwd(cwd, sizeof cwd)) return 8;
    time_t now = time(NULL);
    struct tm tm;
    localtime_r(&now, &tm);
    strftime(day, sizeof day, "%Y/%m/%d", &tm);
    strftime(stamp, sizeof stamp, "%Y-%m-%dT%H-%M-%S", &tm);
    snprintf(id, sizeof id, "00000000-0000-4000-8000-%012d", getpid());
    snprintf(dir, sizeof dir, "%s/sessions/%s", home ? home : "$SB/home/.codex", day);
    mkdirs(dir);
    snprintf(path, sizeof path, "%s/rollout-%s-%s.jsonl", dir, stamp, id);
    FILE *r = fopen(path, "w");
    if (!r) return 7;
    fprintf(r, "{\"type\":\"session_meta\",\"payload\":{\"id\":\"%s\",\"cwd\":\"%s\"}}\\n", id, cwd);
    snprintf(text, sizeof text, "carry-me-%d", getpid());
    item(r, "UserMessage", text, NULL);
    snprintf(text, sizeof text, "carried-reply-%d", getpid());
    item(r, "AgentMessage", text, "final_answer");
    fclose(r);

    // the input box: a rule over the prompt line, which is what the
    // switch waits to see before it types at the new codex
    printf("\\n──────────\\n› ");
    fflush(stdout);
    char line[4096];
    while (fgets(line, sizeof line, stdin)) {
        FILE *g = fopen("$CODEX_LOG", "a");
        if (!g) return 9;
        line[strcspn(line, "\\r\\n")] = 0;
        fprintf(g, "input pid=%d: %s\\n", getpid(), line);
        fclose(g);
    }
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
  nohup "$SHELL_BIN" >"$RUNLOG" 2>&1 < /dev/null & dev_record_pid $!
disown

# A cold first exec of each binary can take seconds on a host whose
# Gatekeeper daemon is busy, and the supervisor gives a core that
# misses its HelloAck a second try — so this waits for a pane to
# actually be addressable rather than for a fixed time after boot.
wait_for 'L3_SPAWNED' 90 0 || fail "no pane session was ever spawned"
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
BOUND_BASE="$(mark)"
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
settle() { # no new start for 2 s, within 60 s
  local last=-1 n i
  for (( i = 0; i < 30; i++ )); do
    n="$(starts)"
    [[ "$n" == "$last" ]] && return 0
    last="$n"; sleep 2
  done
  fail "the stand-in kept restarting for 60 s ($(starts) starts)"
}
settle
BEFORE="$(starts)"
LIVE_PID="$(grep '^start ' "$CODEX_LOG" | tail -1 | sed -n 's/.*pid=\([0-9]*\).*/\1/p')"
kill -0 "$LIVE_PID" 2>/dev/null || fail "the stand-in exited on its own (pid $LIVE_PID)"
ok "the stand-in is running as pane $SID's agent (pid $LIVE_PID)"

# The plugin has to SEE it before a menu pick can mean anything: the
# badge is published off its own scan, and a pick on a pane it has not
# bound is dropped.
wait_for "codex.*$(sid_is "$SID")" 30 "$BOUND_BASE" \
  || fail "the plugin never recognised the pane's agent"
ok "the plugin bound the pane to it"

# the tag `handoff::profile_tag` gives codex's P2 row:
# 0x4F48_0000 | owner << 12 | profile, codex = 2
PICK="$SID $(( 0x4F480000 | 2 << 12 | 2 ))"
# Renamed into place, so the tick that reads it cannot catch it
# half-written — a plain `>` redirect once lost the pick that way.
pick() {
  local base; base="$(mark)"
  echo "$PICK" > "$MENU_FILE.tmp" && mv "$MENU_FILE.tmp" "$MENU_FILE"
  wait_for 'DEV_BADGE_MENU' 20 "$base" || fail "the seam never fired"
}
HELD="switch\.held.*pane $SID: BadgeCodexTerminalBusy"
HANDED="switch\.no_thread.*pane $SID: thread not known, handing off to P2"

echo "==> picking profile 2 while the terminal has just drawn"
# The pane drew the echoed command line and the stand-in's input box
# moments ago, and no daemon says whether a turn is running: the pick
# is held, not acted on.
QUIET_BASE="$(mark)"
base="$(mark)"
pick
wait_for "$HELD" 10 "$base" \
  || fail "a pick on a terminal that just drew was not held as busy"
sleep 1
(( $(starts) == BEFORE )) || fail "a held pick still started a codex"
kill -0 "$LIVE_PID" 2>/dev/null || fail "a held pick still ended the running codex"
ok "the pick was held: the terminal had just drawn"

echo "==> waiting for the pane to read quiet"
# The pane state machine calls a silent foreground job quiet after
# 30 s without output, and the switch wants it quiet 3 s more.  Nothing
# logs the second part, so the switch itself is asked: a pick that is
# still held is picked again, and the first one handed off ends the
# wait.  Bounded well past the 3 s, so a threshold that grew would fail.
wait_for "pane_state\.changed.*$(sid_is "$SID").*to=busy:fg_job" 45 "$QUIET_BASE" \
  || fail "the pane never settled into a quiet foreground job"
SETTLED_AT=$SECONDS

echo "==> picking profile 2 from the badge menu"
HANDOFF_BASE=""
held=0
while (( SECONDS - SETTLED_AT < 15 )); do
  base="$(mark)"
  pick
  for (( i = 0; i < 25; i++ )); do
    if after "$base" | grep -qE "$HANDED"; then HANDOFF_BASE=$base; break 2; fi
    if after "$base" | grep -qE "$HELD"; then held=$(( held + 1 )); break; fi
    sleep 0.2
  done
  (( i < 25 )) || fail "a pick on a quiet pane was neither held nor handed off"
  sleep 0.5
done
[[ -n "$HANDOFF_BASE" ]] \
  || fail "the pick on a quiet pane was not handed off within 15 s ($held picks held)"
ok "the pick on a quiet pane was handed off ($held picks held while the 3 s ran)"

# What the switch must do: end the running one, and start codex under
# the other profile's CODEX_HOME.
for (( i = 0; i < 200; i++ )); do
  (( $(starts) > BEFORE )) && break
  sleep 0.2
done
(( $(starts) > BEFORE )) || fail "no codex was started after the pick"
kill -0 "$LIVE_PID" 2>/dev/null && fail "the codex that was running is still running"
ok "the codex that was running was ended"

SECOND="$(grep '^start ' "$CODEX_LOG" | tail -1)"
NEW_PID="$(sed -n 's/.*pid=\([0-9]*\).*/\1/p' <<<"$SECOND")"
grep -q "CODEX_HOME=$HOME/.codex-profile-2" <<<"$SECOND" \
  || fail "the new codex did not get profile 2's home: $SECOND"
ok "it came back under profile 2's CODEX_HOME"
# profile 2 has never held this pane's conversation, so there is
# nothing there to resume: the conversation travels as a handoff
grep -q 'argv=$' <<<"$SECOND" \
  || fail "with no session on profile 2, the new codex should start fresh: $SECOND"
ok "with no session on profile 2, it started fresh"

echo "==> waiting for the handoff to be typed at the new codex"
wait_for "handoff\.switch\.paste.*$(sid_is "$SID")" 60 "$HANDOFF_BASE" \
  || fail "the handoff never pasted anything into pane $SID"
for (( i = 0; i < 50; i++ )); do
  grep -q "^input pid=$NEW_PID: " "$CODEX_LOG" && break
  sleep 0.2
done
# a second paste or a stray newline would land after the first line:
# give it a moment before counting
sleep 1
TYPED="$(grep "^input pid=$NEW_PID: " "$CODEX_LOG")"
[[ -n "$TYPED" ]] || fail "the new codex (pid $NEW_PID) was typed nothing"
[[ "$(wc -l <<<"$TYPED" | tr -d ' ')" == 1 ]] || fail "the new codex was typed more than one line: $TYPED"
grep -qF '[marspot handoff] I just switched this pane from codex to you' <<<"$TYPED" \
  || fail "what the new codex was typed is not the handoff message: $TYPED"
ok "the new codex was typed the handoff message"
grep -q "^input pid=$LIVE_PID: " "$CODEX_LOG" \
  && fail "the codex that was leaving was typed at: $(grep "^input pid=$LIVE_PID: " "$CODEX_LOG")"

DOC="$MARSPOT_STATE_DIR/handoff/$SID.md"
grep -qF " is in $DOC. Read it" <<<"$TYPED" \
  || fail "the message does not name pane $SID's handoff document $DOC: $TYPED"
[[ -f "$DOC" ]] || fail "the document the message names does not exist: $DOC"
grep -qF "carry-me-$LIVE_PID" "$DOC" && grep -qF "carried-reply-$LIVE_PID" "$DOC" \
  || fail "the document does not carry the leaving codex's turn: $(head -c 600 "$DOC")"
ok "the document it names carries the turn from the leaving codex's rollout"

echo "PASS — a badge-menu pick moves a live codex pane to another profile, conversation and all"
