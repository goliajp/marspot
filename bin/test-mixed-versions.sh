#!/usr/bin/env bash
#
# This build and an older one of the app, running against each other's
# sessions, with the real binaries of both.
#
# What a session publishes through shared memory is wire between the
# core and the session, and across updates: a pane has to survive each
# of these with the same session process, the same shell and its
# screen.
#
#   update    old app → new core over old sessions → the sessions move
#             to the new build by their own execv → the core restarts
#             → the core is rolled back to the old one, which reads a
#             region the new session wrote its identity into
#   rollback  new app → old core over new sessions: the pane lives with
#             its text; the old core finds no cluster text and no
#             title for it, which is the accepted cost until the core
#             is the new one again → new core
#   lost      old app whose session's grid region lost its name → new
#             core with a new session build staged: the session
#             updates, moves to a new region and lives → and with no
#             session build staged, the one case in which the session
#             is started again
#
#   MARSPOT_OLD_BINS=<dir>   the older build: marspot-shell, -core and
#                            -session, and examples/dump_row and
#                            examples/shm_probe built from that tree
#
# Run after `cargo build --release`; exits 0 on success.  Every check
# runs even after one failed, so one run shows all of them.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/bin/_dev-sandbox.sh"
source "$ROOT/bin/_sandbox-panes.sh"
NEW="$ROOT/target/release"
OLD="${MARSPOT_OLD_BINS:-}"
PROBE="$NEW/examples/shm_probe"
SD="$MARSPOT_STATE_DIR"

die() {
  echo "FAIL: $*"
  echo "  (last 25 events of $SD):"
  tail -25 "$(log_of "$SD")" 2>/dev/null | cut -c1-240 | sed 's/^/    /'
  exit 1
}

cleanup() { dev_kill_shell_core; }
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

[[ -n $OLD ]] || die "MARSPOT_OLD_BINS is not set: this suite needs an older build to run against"
for b in marspot-shell marspot-core marspot-session examples/dump_row examples/shm_probe; do
  [[ -x "$OLD/$b" ]] || die "no $OLD/$b"
done
( cd "$ROOT" && cargo build --release --example shm_probe --example dump_row 2>&1 | tail -3 )
[[ -x "$PROBE" && -x "$NEW/examples/dump_row" ]] || die "the probes did not build"
for b in marspot-shell marspot-core marspot-session; do
  MARSPOT_NO_REDIRECT=1 "$OLD/$b" --version >/dev/null 2>&1
done
echo "old build: $(MARSPOT_NO_REDIRECT=1 "$OLD/marspot-core" --version 2>/dev/null)  new build: $(MARSPOT_NO_REDIRECT=1 "$NEW/marspot-core" --version 2>/dev/null)"

FLAG=$'\360\237\207\257\360\237\207\265'
SID="" PID="" SHELL_PID=""

# A pane with a mark, a flag and a title on it, in a fresh sandbox
# started from the app at `$1`.
start_app() { # directory of the build to start
  cleanup
  CLI="$1/marspot-shell"
  launch "$SD" "$1/marspot-shell"
  mkdir -p "$SD/work"
  printf '\033]2;TITLE-M\007MARK-M \360\237\207\257\360\237\207\265\n' > "$SD/work/m.txt"
  SID=$(first_pane "$SD") || die "no pane"
  pane_ready "$SD" "$SID"
  type_in_pane "$SD" "$SID" "cat $SD/work/m.txt"
  wait_shows "$SD" "$SID" MARK-M 10 || die "the pane never showed what was printed in it"
  PID=$(entry "$SD" "$SID" pid)
  SHELL_PID=$(entry "$SD" "$SID" shell_child_pid)
  echo "  session $SID pid $PID shell $SHELL_PID region $(entry "$SD" "$SID" shm_name)"
}

same_pane() {
  check "the session is the same process" test "$(entry "$SD" "$SID" pid)" = "$PID"
  check "the session process is alive" alive "$PID"
  check "the pane's shell is alive" alive "$SHELL_PID"
  check "the app reads the pane's text" shows "$SD" "$SID" MARK-M
}
lacks_log() { ! logged "$SD" "$1"; }
frame_shows() { "$PROBE" screen "$SD/sessions/$SID" 2>/dev/null | grep -qF -- "$1"; }
new_readers_read_everything() {
  check "this build reads the frame" frame_shows MARK-M
  check "this build reads the cluster text" frame_shows "$FLAG"
  check "this build reads the title" test -n "$("$PROBE" title "$SD/sessions/$SID" 2>/dev/null)"
}

# Type a line, wait for it in the frame as this build reads it, and
# compare that frame with the one the older build's reader takes from
# the same region.
frames_match_old_reader() { # how many frames
  local i n=$1 new old tries ok=1
  for (( i = 1; i <= n; i++ )); do
    type_in_pane "$SD" "$SID" "echo FRAME-\$((100+$i))"
    for (( tries = 0; tries < 50; tries++ )); do
      new=$("$NEW/examples/dump_row" "$SD/sessions/$SID" 40 2>/dev/null)
      old=$("$OLD/examples/dump_row" "$SD/sessions/$SID" 40 2>/dev/null)
      [[ $new == *"FRAME-$((100 + i))"* && $new == "$old" ]] && break
      sleep 0.2
    done
    if [[ $new != *"FRAME-$((100 + i))"* || $new != "$old" ]]; then
      ok=0
      echo "    frame $i differs or never arrived; old reader: $(head -c 200 <<<"$old" | tr '\n' '|')"
    fi
  done
  (( ok == 1 ))
}

echo "════ update: old app, then the new core over old sessions ════"
start_app "$OLD"
swap_core "$SD" "$NEW/marspot-core"
sleep 1
same_pane
check "the core took the session over" test "$(count "$SD" L3_REATTACHED)" -ge 1
check "the session was not moved or ended for it" test "$(count "$SD" L3_REHOUSE)" -eq 0
check "no session was started again" lacks_log core.reattach.l3_failed
check "the old session says nothing about identities" test -z "$(entry "$SD" "$SID" stamps_regions)"
new_readers_read_everything

echo "════ update: the sessions move to the new build ════"
execs=$(count "$SD" L3_EXECV_INVOKE)
swap_core "$SD" "$NEW/marspot-core" "$NEW/marspot-session"
wait_count "$SD" L3_EXECV_INVOKE "$execs" 120 || die "the session never moved to the new build"
for (( i = 0; i < 100; i++ )); do [[ -n $(entry "$SD" "$SID" stamps_regions) ]] && break; sleep 0.2; done
sleep 1
same_pane
check "the session runs the new build" test "$(entry "$SD" "$SID" stamps_regions)" = 1
check "the session kept its region" test "$(count "$SD" L3_REHOUSE)" -eq 0
new_readers_read_everything
check "the frames keep coming" frames_match_old_reader 1

echo "════ update: the new core restarts over new sessions ════"
swap_core "$SD" "$NEW/marspot-core"
sleep 1
same_pane
check "the session kept its region" test "$(count "$SD" L3_REHOUSE)" -eq 0
new_readers_read_everything

echo "════ update: the core is rolled back to the old one ════"
swap_core "$SD" "$OLD/marspot-core"
sleep 1
same_pane
check "no session was started again" lacks_log core.reattach.l3_failed
check "the old reader takes every frame as this build does" frames_match_old_reader 5
check "the old readers find the cluster text and title where they look" \
  test -n "$("$OLD/examples/shm_probe" title "$SD/sessions/$SID" 2>/dev/null)"

echo "════ rollback: new app, then the old core over new sessions ════"
start_app "$NEW"
swap_core "$SD" "$OLD/marspot-core"
sleep 1
same_pane
check "no session was started again" lacks_log core.reattach.l3_failed
check "the old reader takes every frame as this build does" frames_match_old_reader 5
check "the old readers find no title for the pane (the accepted cost)" \
  test -z "$("$OLD/examples/shm_probe" title "$SD/sessions/$SID" 2>/dev/null)"
new_readers_read_everything

echo "════ rollback: the core is the new one again ════"
swap_core "$SD" "$NEW/marspot-core"
sleep 1
same_pane
check "the session kept its region" test "$(count "$SD" L3_REHOUSE)" -eq 0
new_readers_read_everything

echo "════ lost: an old session without its region, a new session build staged ════"
start_app "$OLD"
"$PROBE" unlink "$SD/sessions/$SID"
swap_core "$SD" "$NEW/marspot-core" "$NEW/marspot-session"
wait_count "$SD" L3_REHOUSED 0 120
same_pane
check "the session updated and moved to a region made for it" test "$(count "$SD" L3_REHOUSED)" -ge 1
check "no session was started again" lacks_log core.reattach.l3_failed
new_readers_read_everything

echo "════ lost: an old session without its region, nothing to update to ════"
start_app "$OLD"
"$PROBE" unlink "$SD/sessions/$SID"
swap_core "$SD" "$NEW/marspot-core"
sleep 2
check "the session was started again in its place" logged "$SD" core.reattach.l3_failed
check "the old session process is gone" test "$(entry "$SD" "$SID" pid)" != "$PID"
check "the pane has a session" alive "$(entry "$SD" "$SID" pid)"

echo
if (( panes_failures > 0 )); then
  echo "  (reattach, move and update events of the last phase)"
  grep -E $'\t(L3_REATTACHED|L3_REHOUSE[D]?|L3_REPUBLISHED|L3_EXECV_INVOKE|L3_SWAP_ASK|core\.reattach[a-z_.0-9]*|l3\.republish[a-z_.0-9]*)\t' \
    "$(log_of "$SD")" | cut -c1-220 | sed 's/^/    /'
  echo "FAIL: $panes_failures check(s)"
  exit 1
fi
echo "ALL PASS"
