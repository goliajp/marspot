#!/usr/bin/env bash
#
# The default model all Claude Code accounts share, guarded, end to end
# in a sandboxed app.  The settings file is a sandbox copy that a
# profile directory links to, named by MARSPOT_TEST_CC_SETTINGS; the
# pane runs a stand-in for Claude Code's input box (_fake_claude_box.py)
# whose /model writes that file.  Keys come from the dev seam that plays
# them into the core as the window would (MARSPOT_DEV_PANELS).
#
#   1. a model key written into the file is logged, told over the pane
#      and put back within a second; another key is told and kept;
#   2. Enter on /model in the box is held and a notice shown; y sends it,
#      and the model it writes is kept;
#   3. Esc drops the held Enter and the box keeps /model;
#   4. a program's /model (pane send) is refused with a reason;
#   5. with claude.model = manual nothing is held, refused or put back.
#
# Sandbox-only: MARSPOT_STATE_DIR and HOME are redirected and the
# installed app is never touched.  Opens a window, so it runs on the
# runner:  bin/test-e2e-remote.sh test-model-guard.sh
# Exits 0 when every check passed.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
source "$ROOT/bin/_sandbox-panes.sh"

SD="$MARSPOT_STATE_DIR"
BIN="$ROOT/target/release"
CLI="$BIN/marspot-shell"
PEER="$BIN/marspot-peer"
LOG="$SD/logs/marspot.log"
SB=/tmp/marspot-model-guard-$$
SHARED="$SB/claude-shared/settings.json"
SUBMITS="$SB/submits.log"
KEYS="$SB/keys.log"

die() {
  echo "FAIL: $*"
  echo "  --- guard lines in $LOG:"
  grep -aE 'cc_model|MODEL_GUARD|NOTICE|PANIC' "$LOG" 2>/dev/null | tail -30 | cut -c1-300 | sed 's/^/    /'
  exit 1
}
cleanup() {
  local rc=$?
  dev_kill_shell_core
  (( rc != 0 )) || rm -rf "${SB:?}"
}
trap cleanup EXIT

for b in marspot-shell marspot-core marspot-session marspot-peer; do
  [[ -x $BIN/$b ]] || die "$BIN/$b is not built"
done

rm -rf "${SB:?}"
mkdir -p "$SB/claude-shared" "$SB/home/.claude-profile-1"
printf '{\n  "theme": "dark"\n}\n' > "$SHARED"
ln -s "$SHARED" "$SB/home/.claude-profile-1/settings.json"
export HOME="$SB/home" MARSPOT_TEST_CC_SETTINGS="$SB/home/.claude-profile-1/settings.json"

pc() { in_sd "$SD" "$PEER" pane "$@"; }
model() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("model", ""))' "$SHARED"; }
setting() { printf 'claude.model = %s\nreclaim.enabled = false\n' "$1" > "$SD/settings.toml"; }
# A writer that replaces the file by rename, as Claude Code's /model does.
write_shared() { # json text
  printf '%s\n' "$1" > "$SHARED.new" && mv "$SHARED.new" "$SHARED"
}
within() { # seconds, command…: true once the command is, polling every 0.1 s
  local n=$(( $1 * 10 )) i
  shift
  for (( i = 0; i < n; i++ )); do "$@" && return 0; sleep 0.1; done
  "$@"
}
model_is() { [[ $(model) == "$1" ]]; }
more_than() { (( $(count "$SD" "$1") > $2 )); }
submitted() { grep -qxF -- "$1" "$SUBMITS" 2>/dev/null; }
submits() { cat "$SUBMITS" 2>/dev/null | grep -c . || true; }
go() { touch "$SD/$1"; }

start_app() { # dev panel script
  local i
  dev_kill_shell_core
  dev_wipe_state
  mkdir -p "$SD/logs"
  setting managed
  env MARSPOT_SESSION_BIN="$BIN/marspot-session" MARSPOT_CORE_BIN="$BIN/marspot-core" MARSPOT_DEV_PANELS="$1" \
    nohup "$CLI" >> "$(dev_run_log model-guard)" 2>&1 < /dev/null &
  dev_record_pid $!
  wait_count "$SD" HELLO_ACK 0 90 || die "the core never said hello"
  wait_count "$SD" PANE_CONTROL_CAPS_SEEN 0 30 || die "the shell never heard what the core does with pane control"
  P1=""
  for (( i = 0; i < 100; i++ )); do
    P1=$(pc list --json 2>/dev/null | python3 -c 'import json,sys; ps=json.load(sys.stdin)["result"]["panes"]; print(ps[0]["id"] if ps else "")' 2>/dev/null)
    [[ -n $P1 ]] && break
    sleep 0.5
  done
  [[ -n $P1 ]] || die "no pane came up"
  for (( i = 0; i < 30; i++ )); do pc run "$P1" --timeout 10 true >/dev/null 2>&1 && break; sleep 0.5; done
  wait_count "$SD" svc.cc_model.watching 0 30 || die "the shared settings are not watched"
}

typed() { local s="" c; for (( c = 0; c < ${#1}; c++ )); do s+="type:${1:c:1};"; done; echo "$s"; }
script="until:go-2;$(typed /model)key:enter;wait:1500;until:go-2y;type:y;wait:1500"
script+=";until:go-3;$(typed /model)key:enter;wait:1500;until:go-3esc;esc;wait:1500;until:go-3x;type:x;wait:800;key:ctrl-u;wait:800"
script+=";until:go-5;$(typed /model)key:enter;wait:1500"
start_app "$script"

echo "════ 1. a model written into the shared file ════"
put=$(count "$SD" svc.cc_model.put_back) shown=$(count "$SD" NOTICE_SHOWN)
write_shared '{"theme": "dark", "model": "fable"}'
check "it is put back within a second" within 1 model_is ""
check "the change is logged" within 1 more_than svc.cc_model.put_back "$put"
check "and told over the pane within a second" within 1 more_than NOTICE_SHOWN "$shown"
check "the profile's file is still a link" test -L "$SB/home/.claude-profile-1/settings.json"
shown=$(count "$SD" NOTICE_SHOWN) put=$(count "$SD" svc.cc_model.put_back)
write_shared '{"theme": "light"}'
check "another key is told" within 1 more_than NOTICE_SHOWN "$shown"
sleep 1
check "and kept" grep -q light "$SHARED"
check "and not put back" test "$(count "$SD" svc.cc_model.put_back)" = "$put"

echo "════ 2. /model typed in the box ════"
pc send --force "$P1" "python3 $ROOT/bin/_fake_claude_box.py $SHARED $SUBMITS $KEYS" >/dev/null || die "could not start the stand-in"
wait_shows "$SD" "$P1" "? for shortcuts" 20 || die "the stand-in's box never showed"
held=$(count "$SD" MODEL_GUARD_HELD) shown=$(count "$SD" NOTICE_SHOWN)
go go-2
check "Enter on /model is held" within 5 more_than MODEL_GUARD_HELD "$held"
sleep 1
check "nothing reached the program" test "$(submits)" = 0
check "the box still holds /model" shows "$SD" "$P1" "❯ /model"
kept=$(count "$SD" svc.cc_model.changed)
go go-2y
check "y sends it" within 5 submitted /model
check "the shell heard it was confirmed" within 5 logged "$SD" "svc.cc_model.confirmed"
check "the model it wrote is kept" within 3 more_than svc.cc_model.changed "$kept"
sleep 1
check "and still there" model_is fable

echo "════ 3. Esc ════"
n=$(submits) dropped=$(count "$SD" MODEL_GUARD_DROPPED) esc_in=$(grep -c '^1b$' "$KEYS" 2>/dev/null || true)
go go-3
check "held again" within 5 more_than MODEL_GUARD_HELD "$held"
go go-3esc
check "Esc drops it" within 5 more_than MODEL_GUARD_DROPPED "$dropped"
sleep 1
check "nothing was sent" test "$(submits)" = "$n"
check "the Esc did not reach the program" test "$(grep -c '^1b$' "$KEYS" 2>/dev/null || true)" = "$esc_in"
check "the box keeps /model" shows "$SD" "$P1" "❯ /model"
go go-3x
check "after it a key is typed again" within 5 shows "$SD" "$P1" "❯ /modelx"
sleep 1.5

echo "════ 4. a program's /model ════"
check "the box is empty again" within 5 lacks "$SD" "$P1" "❯ /model"
out=$(pc send --force "$P1" "/model opus" 2>&1)
rc=$?
check "pane send is refused" test "$rc" != 0
check "with the reason ($out)" grep -q 'only the keyboard can confirm' <<<"$out"
sleep 1
check "nothing was sent" test "$(submits)" = "$n"
check "the model is unchanged" model_is fable

echo "════ 5. manual ════"
setting manual
sleep 2
held=$(count "$SD" MODEL_GUARD_HELD) put=$(count "$SD" svc.cc_model.put_back)
go go-5
check "the keyboard's /model is sent at once" within 5 submitted "/model"
check "and not held" test "$(count "$SD" MODEL_GUARD_HELD)" = "$held"
n=$(submits)
out=$(pc send --force "$P1" "/model sonnet" 2>&1)
check "a program's is sent too ($out)" within 5 submitted "/model sonnet"
sleep 1
check "and the model it wrote is kept" model_is sonnet
check "nothing was put back" test "$(count "$SD" svc.cc_model.put_back)" = "$put"
pc send --force "$P1" $'\x03' >/dev/null 2>&1 || true

echo
echo "$panes_checks checks, $panes_failures failed"
(( panes_failures == 0 ))
