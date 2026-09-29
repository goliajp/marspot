#!/usr/bin/env bash
#
# Handing a pane's conversation between claude and codex (RFC-009),
# end to end, there and back and there again.
#
# The unit tests cover each piece — reading both history formats, the
# document, the ledger, the op — against a fake world.  This runs the
# chain: a real pty, processes the plugins recognise, a real SIGTERM, a
# command line typed into a real shell, and a paste arriving in the
# program that took over.
#
# Neither real CLI is in the chain.  They would sign into the user's
# accounts and end their sessions.  Stand-ins take their place and do
# what this depends on them doing: say which session they are on
# (claude's `sessions/<pid>.json`), write their history in each CLI's
# own shape, and record every line they are given.
#
# What is asserted, per switch:
#   - the agent that was running is gone, the other one is up under the
#     picked profile;
#   - the first time it starts fresh, every later time it RESUMES the
#     session it had — so the pane never has more than one session per
#     agent;
#   - it is handed a one-line marked message naming the pane's document;
#   - the document holds what happened on the other side since, and not
#     what was handed over before, nor the other side's own handoff;
#   - the pane has exactly one ledger and one document, and a closed
#     pane's files are removed.
#
# Sandbox-only: MARSPOT_STATE_DIR *and* HOME are redirected.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"

SB=/tmp/marspot-agent-handoff
RUNLOG=$SB/run.log
APPLOG="$MARSPOT_STATE_DIR/logs/marspot.log"
AGENT_LOG=$SB/agents.log
MENU_FILE=$SB/badge-menu
HANDOFF="$MARSPOT_STATE_DIR/handoff"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"

# Badge-menu tags, as `handoff::switch::tag_for` builds them:
# 0x4F48_0000 | agent << 8 | profile (claude = 1, codex = 2).
TAG_TO_CLAUDE_P1=$(( 0x4F480000 | 1 << 8 | 1 ))
TAG_TO_CODEX_P1=$(( 0x4F480000 | 2 << 8 | 1 ))

fail() {
  echo "FAIL: $*"
  echo "  --- agent log:"; sed 's/^/    /' "$AGENT_LOG" 2>/dev/null | cut -c1-240
  echo "  --- handoff dir:"; ls -la "$HANDOFF" 2>/dev/null | sed 's/^/    /'
  echo "  --- last 30 shell lines about the switch:"
  grep -E 'handoff|pty_op|badge.changed' "$APPLOG" 2>/dev/null | tail -30 | cut -c1-240 | sed 's/^/    /'
  exit 1
}
ok() { echo "  ✓ $1"; }

cleanup() {
  dev_kill_shell_core
  # By the pids they recorded, never by name: the stand-ins run as
  # `claude` and `codex`, and a pkill by name would end the user's
  # real sessions.
  if [[ -f "$AGENT_LOG" ]]; then
    while read -r pid; do
      [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null
    done < <(sed -n 's/^start agent=[a-z]* pid=\([0-9]*\).*/\1/p' "$AGENT_LOG")
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
wait_file() { # regex over the agent log, seconds
  local re="$1" secs="$2" i
  for (( i = 0; i < secs * 5; i++ )); do
    grep -qE "$re" "$AGENT_LOG" 2>/dev/null && return 0
    sleep 0.2
  done
  return 1
}

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" || ! -x "$SESSION_BIN" ]]; then
  echo "==> building release binaries"
  ( cd "$ROOT" && cargo build --release \
      --bin marspot-shell --bin marspot-core --bin marspot-session 2>&1 | tail -3 )
fi

# ── the stand-ins ─────────────────────────────────────────────────
#
# One program, two names: the plugins identify an agent by argv[0]'s
# basename, so it is compiled and copied rather than scripted (a `#!`
# script runs as `sh`).  The log path is compiled in because a pane
# does not receive the launcher's MARSPOT_/CODEX_ variables.
echo "==> building the stand-ins"
rm -rf "$SB"; mkdir -p "$SB/bin"
cat > "$SB/agent-stub.c" <<STUB
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/stat.h>

static void mkdirs(const char *path) {
    char tmp[4096];
    snprintf(tmp, sizeof tmp, "%s", path);
    for (char *p = tmp + 1; *p; p++) {
        if (*p == '/') { *p = 0; mkdir(tmp, 0755); *p = '/'; }
    }
    mkdir(tmp, 0755);
}

static void esc(FILE *f, const char *s) {
    for (; *s; s++) {
        if (*s == '"' || *s == '\\\\') fputc('\\\\', f);
        if ((unsigned char)*s >= 0x20) fputc(*s, f);
    }
}

int main(int argc, char **argv) {
    FILE *log = fopen("$AGENT_LOG", "a");
    if (!log) return 9; // nothing observed must not look like nothing happened
    const char *name = strrchr(argv[0], '/'); name = name ? name + 1 : argv[0];
    int claude = strcmp(name, "claude") == 0;
    const char *home = getenv(claude ? "CLAUDE_CONFIG_DIR" : "CODEX_HOME");
    if (!home) { fprintf(log, "start agent=%s pid=%d NO-HOME\\n", name, getpid()); return 8; }
    const char *resume = NULL;
    for (int i = 1; i + 1 < argc; i++)
        if (strcmp(argv[i], claude ? "--resume" : "resume") == 0) resume = argv[i + 1];
    char id[64];
    if (resume) snprintf(id, sizeof id, "%s", resume);
    else snprintf(id, sizeof id, "%08x-0000-4000-8000-%012x", getpid(), claude ? 0xc1 : 0xc0de);
    char cwd[2048]; getcwd(cwd, sizeof cwd);

    fprintf(log, "start agent=%s pid=%d home=%s id=%s argv=", name, getpid(), home, id);
    for (int i = 1; i < argc; i++) fprintf(log, "%s%s", i > 1 ? " " : "", argv[i]);
    fprintf(log, "\\n"); fflush(log);

    char hist[4096], dir[4096];
    if (claude) {
        char enc[2048]; snprintf(enc, sizeof enc, "%s", cwd);
        for (char *p = enc; *p; p++) if (*p == '/') *p = '-';
        snprintf(dir, sizeof dir, "%s/projects/%s", home, enc); mkdirs(dir);
        snprintf(hist, sizeof hist, "%s/%s.jsonl", dir, id);
        // The CLI's own word on which session this process is on.
        char sdir[4096], rec[4096]; snprintf(sdir, sizeof sdir, "%s/sessions", home); mkdirs(sdir);
        snprintf(rec, sizeof rec, "%s/%d.json", sdir, getpid());
        FILE *r = fopen(rec, "w");
        fprintf(r, "{\\"pid\\":%d,\\"sessionId\\":\\"%s\\",\\"cwd\\":\\"%s\\",\\"kind\\":\\"interactive\\",\\"status\\":\\"idle\\"}", getpid(), id, cwd);
        fclose(r);
    } else {
        snprintf(dir, sizeof dir, "%s/sessions/2026/09/22", home); mkdirs(dir);
        snprintf(hist, sizeof hist, "%s/rollout-2026-09-22T00-00-00-%s.jsonl", dir, id);
        FILE *h = fopen(hist, "a");
        fprintf(h, "{\\"type\\":\\"turn_context\\",\\"payload\\":{\\"cwd\\":\\"%s\\",\\"model\\":\\"stub\\"}}\\n", cwd);
        fclose(h);
    }
    // A first frame's worth of output: the switch waits for kilobytes
    // before it counts the screen as drawn.
    for (int i = 0; i < 60; i++) printf("%s stand-in ready, session %s ..........\\n", name, id);
    fflush(stdout);

    char line[8192], prev[64] = "";
    for (int n = 1; fgets(line, sizeof line, stdin); n++) {
        line[strcspn(line, "\\r\\n")] = 0;
        if (!*line) { n--; continue; }
        fprintf(log, "input agent=%s pid=%d text=%s\\n", name, getpid(), line); fflush(log);
        FILE *h = fopen(hist, "a");
        if (claude) {
            fprintf(h, "{\\"uuid\\":\\"%d-%da\\",\\"parentUuid\\":", getpid(), n);
            if (*prev) fprintf(h, "\\"%s\\"", prev); else fprintf(h, "null");
            fprintf(h, ",\\"isSidechain\\":false,\\"type\\":\\"user\\",\\"message\\":{\\"role\\":\\"user\\",\\"content\\":\\"");
            esc(h, line);
            fprintf(h, "\\"}}\\n{\\"uuid\\":\\"%d-%db\\",\\"parentUuid\\":\\"%d-%da\\",\\"isSidechain\\":false,\\"type\\":\\"assistant\\",\\"message\\":{\\"role\\":\\"assistant\\",\\"model\\":\\"claude-opus-5\\",\\"content\\":[{\\"type\\":\\"text\\",\\"text\\":\\"claude answered: ", getpid(), n, getpid(), n);
            esc(h, line);
            fprintf(h, "\\"}]}}\\n");
            snprintf(prev, sizeof prev, "%d-%db", getpid(), n);
        } else {
            fprintf(h, "{\\"type\\":\\"event_msg\\",\\"payload\\":{\\"type\\":\\"item_completed\\",\\"item\\":{\\"type\\":\\"UserMessage\\",\\"content\\":[{\\"type\\":\\"text\\",\\"text\\":\\"");
            esc(h, line);
            fprintf(h, "\\"}]}}}\\n{\\"type\\":\\"event_msg\\",\\"payload\\":{\\"type\\":\\"item_completed\\",\\"item\\":{\\"type\\":\\"AgentMessage\\",\\"phase\\":\\"final_answer\\",\\"content\\":[{\\"type\\":\\"Text\\",\\"text\\":\\"codex answered: ");
            esc(h, line);
            fprintf(h, "\\"}]}}}\\n{\\"type\\":\\"turn_context\\",\\"payload\\":{\\"cwd\\":\\"%s\\",\\"model\\":\\"stub\\"}}\\n", cwd);
        }
        fclose(h);
        printf("%s: got it\\n", name); fflush(stdout);
    }
    return 0;
}
STUB
cc -O0 -o "$SB/bin/claude" "$SB/agent-stub.c" || { echo "cc failed"; exit 1; }
cp "$SB/bin/claude" "$SB/bin/codex"

# ── this test's own HOME ──────────────────────────────────────────
export HOME="$SB/home"
mkdir -p "$HOME/.claude-profile-1" "$HOME/.codex-profile-1"
ln -sfn "$HOME/.codex-profile-1" "$HOME/.codex"
printf 'model = "stub"\n' > "$HOME/.codex-profile-1/config.toml"
export PATH="$SB/bin:$PATH"
: > "$AGENT_LOG"

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
[[ -n "${SID:-}" ]] || fail "no pane to work with"
echo "==> pane $SID"

# Wait for the shell to read before typing anything that matters: a
# line typed into the gap sits in the pty and runs later, and the
# switch would get the blame (see test-codex-profile-switch.sh).
READY="$SB/pane-ready"
for (( attempt = 1; attempt <= 10; attempt++ )); do
  "$SHELL_BIN" --send "$SID" "echo reading > $READY" >/dev/null 2>&1 || fail "--send refused"
  for (( i = 0; i < 15; i++ )); do [[ -f "$READY" ]] && break 2; sleep 0.2; done
done
[[ -f "$READY" ]] || fail "the pane's shell never ran anything typed into it"

send() { # text — retried only while REFUSED (a refusal queues nothing)
  local i
  for (( i = 0; i < 40; i++ )); do
    "$SHELL_BIN" --send "$SID" "$1" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  fail "--send was refused 40 times: $1"
}
starts() { grep -c "^start agent=$1 " "$AGENT_LOG"; }
last_start() { grep "^start agent=$1 " "$AGENT_LOG" | tail -1; }
pid_of() { sed -n 's/.* pid=\([0-9]*\) .*/\1/p' <<<"$1"; }
id_of() { sed -n 's/.* id=\([^ ]*\) .*/\1/p' <<<"$1"; }
pick() { # tag
  echo "$SID $1" > "$MENU_FILE.tmp" && mv "$MENU_FILE.tmp" "$MENU_FILE"
}
# The paste the switch sends, as the new agent received it.
handoff_input() { # agent pid
  grep "^input agent=$1 pid=$2 text=\[marspot handoff\]" "$AGENT_LOG" | tail -1
}
switch_to() { # agent tag
  local to="$1" tag="$2" before
  before="$(starts "$to")"
  pick "$tag"
  for (( i = 0; i < 150; i++ )); do (( $(starts "$to") > before )) && break; sleep 0.2; done
  (( $(starts "$to") > before )) || fail "no $to was started after the pick"
}

# ── 1. claude, one exchange ───────────────────────────────────────
echo "==> starting claude in the pane"
send "CLAUDE_CONFIG_DIR=$HOME/.claude-profile-1 claude"
wait_file '^start agent=claude ' 20 || fail "the claude stand-in did not start"
C1="$(last_start claude)"; C1_PID="$(pid_of "$C1")"; C_ID="$(id_of "$C1")"
send "first ask about the parser"
wait_file "input agent=claude pid=$C1_PID text=first ask" 10 || fail "claude never got the first ask"
# Bound = the badge names the session.  It only logs when its text
# changes, and it gains the model once the plugin reads the bound
# session's transcript — so that line is the proof of the binding.
binds() { grep -cE "claudecode\.badge\.changed.*sid=$SID .*uuid=\"$C_ID\"" "$APPLOG" 2>/dev/null; }
wait_for "claudecode\.badge\.changed.*sid=$SID .*uuid=\"$C_ID\"" 30 \
  || fail "the claude plugin never bound the pane to session $C_ID"
ok "claude is pane $SID's agent, bound to session $C_ID"

# ── 2. → codex (fresh) ────────────────────────────────────────────
echo "==> hand off to codex P1"
switch_to codex "$TAG_TO_CODEX_P1"
X1="$(last_start codex)"; X1_PID="$(pid_of "$X1")"; X_ID="$(id_of "$X1")"
kill -0 "$C1_PID" 2>/dev/null && fail "claude is still running after the switch"
ok "claude was ended and codex started"
grep -q "home=$HOME/.codex-profile-1 " <<<"$X1" || fail "codex did not get profile 1's home: $X1"
grep -q ' argv=$' <<<"$X1" || fail "the first switch to codex should start it fresh: $X1"
ok "first time to codex: a fresh session ($X_ID)"
wait_file "input agent=codex pid=$X1_PID text=\[marspot handoff\]" 30 || fail "codex was never handed the message"
MSG="$(handoff_input codex "$X1_PID")"
grep -q "$HANDOFF/$SID.md" <<<"$MSG" || fail "the message does not name the pane's document: $MSG"
DOC="$(cat "$HANDOFF/$SID.md")"
grep -q "first ask about the parser" <<<"$DOC" || fail "the document lacks claude's exchange"
grep -q "claude answered: first ask" <<<"$DOC" || fail "the document lacks claude's reply"
ok "codex got the marked message; the document carries claude's exchange"

send "second ask about the tests"
wait_file "input agent=codex pid=$X1_PID text=second ask" 10 || fail "codex never got the second ask"
wait_for "codex\.codex\.badge\.changed.*sid=$SID" 30 \
  || fail "the codex plugin never bound the pane"

# ── 3. → claude (resume) ──────────────────────────────────────────
echo "==> hand off back to claude P1"
switch_to claude "$TAG_TO_CLAUDE_P1"
C2="$(last_start claude)"; C2_PID="$(pid_of "$C2")"
kill -0 "$X1_PID" 2>/dev/null && fail "codex is still running after the switch"
grep -q " argv=--resume $C_ID$" <<<"$C2" || fail "claude should resume $C_ID: $C2"
ok "back to claude: it resumed the same session $C_ID"
wait_file "input agent=claude pid=$C2_PID text=\[marspot handoff\]" 30 || fail "claude was never handed the message"
grep -q "since you left" <<<"$(handoff_input claude "$C2_PID")" || fail "a resumed agent should be told what it missed"
DOC="$(cat "$HANDOFF/$SID.md")"
grep -q "second ask about the tests" <<<"$DOC" || fail "the document lacks codex's exchange"
grep -q "first ask" <<<"$DOC" && fail "the document repeats what claude already had"
grep -q "marspot handoff" <<<"$DOC" && fail "codex's own handoff was handed back"
ok "the document holds only codex's side since claude left"

# A pane that no longer exists, left over from before: the next switch
# must clear it.
echo stale > "$HANDOFF/999999.md"; echo "claude x 1" > "$HANDOFF/999999.ledger"

send "third ask about the docs"
wait_file "input agent=claude pid=$C2_PID text=third ask" 10 || fail "claude never got the third ask"
# The plugin must have re-bound the pane to the RESUMED process before
# the next pick: acting on the old binding would signal a pid that is
# already gone and leave the live claude reading the codex line.
for (( i = 0; i < 150; i++ )); do (( $(binds) >= 2 )) && break; sleep 0.2; done
(( $(binds) >= 2 )) || fail "the claude plugin never re-bound the resumed session"

# ── 4. → codex (resume) ───────────────────────────────────────────
echo "==> hand off to codex P1 again"
switch_to codex "$TAG_TO_CODEX_P1"
X2="$(last_start codex)"; X2_PID="$(pid_of "$X2")"
grep -q " argv=resume $X_ID$" <<<"$X2" || fail "codex should resume $X_ID: $X2"
ok "back to codex: it resumed the same session $X_ID"
wait_file "input agent=codex pid=$X2_PID text=\[marspot handoff\]" 30 || fail "codex was never handed the message"
DOC="$(cat "$HANDOFF/$SID.md")"
grep -q "third ask about the docs" <<<"$DOC" || fail "the document lacks claude's new exchange"
grep -qE "first ask|second ask|marspot handoff" <<<"$DOC" && fail "the document repeats earlier content: $DOC"
ok "the document holds only claude's side since codex left"

# ── garbage ───────────────────────────────────────────────────────
sleep 1
LEFT="$(ls "$HANDOFF" | sort | tr '\n' ' ')"
[[ "$LEFT" == "$SID.ledger $SID.md " ]] || fail "the handoff dir should hold one ledger and one document: $LEFT"
ok "one ledger and one document for the pane; the closed pane's files are gone"
n_claude="$(find "$HOME/.claude-profile-1/projects" -name '*.jsonl' | wc -l | tr -d ' ')"
n_codex="$(find "$HOME/.codex-profile-1/sessions" -name 'rollout-*.jsonl' | wc -l | tr -d ' ')"
[[ "$n_claude" == 1 && "$n_codex" == 1 ]] || fail "sessions: claude=$n_claude codex=$n_codex (want 1 and 1)"
ok "three switches, one session per agent"

echo "PASS — a pane's conversation moves between claude and codex and back, without piling up"
