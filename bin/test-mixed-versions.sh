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
# A new session writes frames of eight-byte cells, under a region
# version no older reader attaches to, but only to a core that said it
# reads them; to any other core, and the moment its core goes, it
# writes the twenty-byte frames every build reads.  Each phase checks
# which of the two the region holds, and an older reader shown a
# region of eight-byte cells has to refuse it rather than misread it.
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
#   repeat    old app → new core and new session build at once, a few
#             times over: every session moves within a bound.  A core
#             that sent an old session a message it does not know,
#             after the request to update, left the session's control
#             reader blocked past its stop check, and the update hung
#             until the core sent anything else
#
# Some of what an older build does depends on whether it is from
# before or after sessions stamped their regions with an identity
# (`stamps_regions` in the registry entry): one from after it names
# its side regions as this build does and moves to a new region when
# asked (`SessionRepublish`), so a pane whose region was lost keeps its
# session even with nothing to update to.  The suite reads which kind
# the older build is from its first session and checks for that.
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
# The build a binary says it is, as the registry entry of a session it
# runs says it.
build_of() { # binary
  # `MARSPOT_FP=<sha>|<time>|END`, which runs straight on into the
  # next marker, after a bare needle in a debug build
  LC_ALL=C grep -ao 'MARSPOT_FP=[^|[:space:]]*|[^|[:space:]]*|END' "$1" | sed 's/|END$//' | head -1
}
OLD_BUILD=$(build_of "$OLD/marspot-session")
NEW_BUILD=$(build_of "$NEW/marspot-session")
[[ -n $OLD_BUILD && -n $NEW_BUILD && $OLD_BUILD != "$NEW_BUILD" ]] ||
  die "the session builds are not two different ones: old '$OLD_BUILD' new '$NEW_BUILD'"
OLD_STAMPS=""
SID="" PID="" SHELL_PID=""
REPEAT="${MIXED_REPEAT:-3}"
SLOW_PROBE="${MIXED_SLOW_PROBE:-2}"
# from the core's settling; the probe of a new image is in it
EXEC_WITHIN="${MIXED_EXEC_WITHIN:-30}"

# A pane with a mark, a flag and a title on it, in a fresh sandbox
# started from the app at `$1`.
start_app() { # directory of the build to start
  cleanup
  # every phase gets the same session id, and an old build names what
  # a session publishes by the id alone: what the last phase's killed
  # session left under those names is not this pane's
  if [[ -n $SID ]]; then dev_forget_regions "/msp-s-$SID" "/msp-c-$SID" "/msp-t-$SID"; fi
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
frame_shows() {
  local out
  out=$("$PROBE" screen "$SD/sessions/$SID" 2>/dev/null)
  [[ $out == *"$1"* ]]
}
# The region's version and cell size, as this build reads them.
layout_is() { # "version cell_size"
  local i out
  for (( i = 0; i < 50; i++ )); do
    out=$("$PROBE" layout "$SD/sessions/$SID" 2>/dev/null)
    [[ $out == "$1" ]] && return 0
    sleep 0.2
  done
  echo "    the region says $out"
  return 1
}
old_reader_refuses() {
  ! "$OLD/examples/dump_row" "$SD/sessions/$SID" 40 >/dev/null 2>&1
}
WIDE="3 20" PACKED="4 8"
# Type a line and wait for it in the frame as this build reads it.
frames_keep_coming() { # how many frames
  local i n=$1 tries
  for (( i = 1; i <= n; i++ )); do
    type_in_pane "$SD" "$SID" "echo FRAME-\$((200+$i))"
    for (( tries = 0; tries < 50; tries++ )); do
      [[ $("$NEW/examples/dump_row" "$SD/sessions/$SID" 40 2>/dev/null) == *"FRAME-$((200 + i))"* ]] && break
      sleep 0.2
    done
    (( tries < 50 )) || return 1
  done
}

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

runs_build() { # build
  local i
  for (( i = 0; i < 100; i++ )); do
    [[ $(entry "$SD" "$SID" build) == "$1" ]] && return 0
    sleep 0.2
  done
  echo "    the entry says $(entry "$SD" "$SID" build)"
  return 1
}

echo "════ update: old app, then the new core over old sessions ════"
start_app "$OLD"
check "the old session's entry names the old build" test "$(entry "$SD" "$SID" build)" = "$OLD_BUILD"
OLD_STAMPS=$(entry "$SD" "$SID" stamps_regions)
echo "  the old build $([[ -n $OLD_STAMPS ]] && echo stamps || echo "does not stamp") its regions"
swap_core "$SD" "$NEW/marspot-core"
sleep 1
same_pane
check "the core took the session over" test "$(count "$SD" L3_REATTACHED)" -ge 1
check "an old session writes twenty-byte frames to the new core" layout_is "$WIDE"
check "the session was not moved or ended for it" test "$(count "$SD" L3_REHOUSE)" -eq 0
check "no session was started again" lacks_log core.reattach.l3_failed
check "the old session's word on identities is its build's" \
  test "$(entry "$SD" "$SID" stamps_regions)" = "$OLD_STAMPS"
new_readers_read_everything

echo "════ update: the sessions move to the new build ════"
execs=$(count "$SD" L3_EXECV_INVOKE)
# whatever the shell's prompt made of it since the file was printed
title_before=$("$PROBE" title "$SD/sessions/$SID" 2>/dev/null)
swap_core "$SD" "$NEW/marspot-core" "$NEW/marspot-session"
wait_count "$SD" L3_EXECV_INVOKE "$execs" 120 || die "the session never moved to the new build"
check "the session runs the new build" runs_build "$NEW_BUILD"
sleep 1
same_pane
check "the session stamps its regions" test "$(entry "$SD" "$SID" stamps_regions)" = 1
check "the session kept its region" test "$(count "$SD" L3_REHOUSE)" -eq 0
check "the title from before the update is still published" \
  test "$("$PROBE" title "$SD/sessions/$SID" 2>/dev/null)" = "$title_before"
new_readers_read_everything
check "the new session writes the new core eight-byte frames" layout_is "$PACKED"
check "the frames keep coming" frames_keep_coming 3
check "an old reader refuses a region of eight-byte frames" old_reader_refuses

echo "════ update: the new core restarts over new sessions ════"
swap_core "$SD" "$NEW/marspot-core"
sleep 1
same_pane
check "the session kept its region" test "$(count "$SD" L3_REHOUSE)" -eq 0
check "the session writes the restarted core eight-byte frames" layout_is "$PACKED"
new_readers_read_everything

echo "════ update: the core is rolled back to the old one ════"
swap_core "$SD" "$OLD/marspot-core"
sleep 1
same_pane
check "no session was started again" lacks_log core.reattach.l3_failed
check "the session writes the old core twenty-byte frames" layout_is "$WIDE"
check "the old reader takes every frame as this build does" frames_match_old_reader 5
check "the old readers find the cluster text and title where they look" \
  test -n "$("$OLD/examples/shm_probe" title "$SD/sessions/$SID" 2>/dev/null)"

echo "════ rollback: new app, then the old core over new sessions ════"
start_app "$NEW"
check "a new app's session writes eight-byte frames" layout_is "$PACKED"
check "an old reader refuses them" old_reader_refuses
swap_core "$SD" "$OLD/marspot-core"
sleep 1
same_pane
check "no session was started again" lacks_log core.reattach.l3_failed
check "the session writes the old core twenty-byte frames" layout_is "$WIDE"
echo "  (moved to a region the old core made: $(count "$SD" L3_REHOUSE) times)"
check "the old reader takes every frame as this build does" frames_match_old_reader 5
if [[ -n $OLD_STAMPS ]]; then
  check "the old readers find the title where this build puts it" \
    test -n "$("$OLD/examples/shm_probe" title "$SD/sessions/$SID" 2>/dev/null)"
else
  check "the old readers find no title for the pane (the accepted cost)" \
    test -z "$("$OLD/examples/shm_probe" title "$SD/sessions/$SID" 2>/dev/null)"
fi
new_readers_read_everything

echo "════ rollback: the core is the new one again ════"
rehoused=$(count "$SD" L3_REHOUSE)
swap_core "$SD" "$NEW/marspot-core"
sleep 1
same_pane
check "the session kept its region" test "$(count "$SD" L3_REHOUSE)" -eq "$rehoused"
check "the session writes the new core eight-byte frames again" layout_is "$PACKED"
new_readers_read_everything

echo "════ rollback: the sessions go back to the old build ════"
execs=$(count "$SD" L3_EXECV_INVOKE)
swap_core "$SD" "$NEW/marspot-core" "$OLD/marspot-session"
wait_count "$SD" L3_EXECV_INVOKE "$execs" 120 || die "the session never moved to the old build"
check "the session runs the old build" runs_build "$OLD_BUILD"
sleep 1
same_pane
check "no session was started again" lacks_log core.reattach.l3_failed
check "an old session writes the new core twenty-byte frames" layout_is "$WIDE"
check "this build reads the frame" frame_shows MARK-M

echo "════ lost: an old session without its region, a new session build staged ════"
start_app "$OLD"
title_before=$("$PROBE" title "$SD/sessions/$SID" 2>/dev/null)
"$PROBE" unlink "$SD/sessions/$SID"
swap_core "$SD" "$NEW/marspot-core" "$NEW/marspot-session"
wait_count "$SD" L3_REHOUSED 0 120
same_pane
check "the session updated and moved to a region made for it" test "$(count "$SD" L3_REHOUSED)" -ge 1
check "no session was started again" lacks_log core.reattach.l3_failed
check "there was a title before the move" test -n "$title_before"
check "the title from before the move came along" \
  test "$("$PROBE" title "$SD/sessions/$SID" 2>/dev/null)" = "$title_before"
new_readers_read_everything

echo "════ lost: an old session without its region, nothing to update to ════"
start_app "$OLD"
"$PROBE" unlink "$SD/sessions/$SID"
swap_core "$SD" "$NEW/marspot-core"
if [[ -n $OLD_STAMPS ]]; then
  wait_count "$SD" L3_REHOUSED 0 120
  check "the old session moved to a region made for it" test "$(count "$SD" L3_REHOUSED)" -ge 1
  same_pane
  check "no session was started again" lacks_log core.reattach.l3_failed
  check "an old session writes the new core twenty-byte frames" layout_is "$WIDE"
else
  sleep 2
  check "the session was started again in its place" logged "$SD" core.reattach.l3_failed
  check "the old session process is gone" test "$(entry "$SD" "$SID" pid)" != "$PID"
  check "the pane has a session" alive "$(entry "$SD" "$SID" pid)"
fi

# An old app with two panes side by side, the second one focused.
start_old_two_panes() {
  local i
  cleanup
  CLI="$OLD/marspot-shell"
  rm -rf "$SD"
  mkdir -p "$SD/logs"
  # shell-state.bin v2: one window, 2x1, two panes that start fresh
  python3 - "$SD" <<'PY' || die "could not seed two panes"
import struct, sys, pathlib
d = pathlib.Path(sys.argv[1])
def s(x):
    b = x.encode()
    return struct.pack('<H', len(b)) + b
def pane():
    return struct.pack('<Q', 0) + s('') + s('')
body = struct.pack('<II', 0xA5505010, 2) + struct.pack('<HH', 0, 1)
body += struct.pack('<HHHH', 2, 1, 1, 2) + pane() + pane()
(d / 'shell-state.bin').write_bytes(body)
PY
  (
    export MARSPOT_STATE_DIR="$SD"
    env MARSPOT_SESSION_BIN="$OLD/marspot-session" nohup "$OLD/marspot-shell" >"$SD/run.log" 2>&1 < /dev/null &
    dev_record_pid $!
  )
  wait_count "$SD" HELLO_ACK 0 90 || die "the old app never said hello"
  for (( i = 0; i < 150; i++ )); do
    SIDS=$(in_sd "$SD" "$CLI" --panes 2>/dev/null | awk 'NR>1 {print $1}')
    (( $(wc -w <<<"$SIDS") == 2 )) && break
    sleep 0.4
  done
  (( $(wc -w <<<"$SIDS") == 2 )) || die "the old app did not come up with two panes: '$SIDS'"
  for sid in $SIDS; do pane_ready "$SD" "$sid"; done
}

echo "════ repeat: old sessions moved to the new build with the new core, $REPEAT times ════"
# The new session build behind a probe as slow as a first launch on an
# installed app: an old session asked to update probes the new image
# and keeps reading its control socket meanwhile, so whatever the core
# sends it after the request lands in that window.  Two panes, because
# the focused one is sent its focus right after, and that frame is
# what used to let a session past a message it did not know.  The
# build marker in the script is the binary's, so the session finds
# another build staged and, once moved, the one it runs.
for (( round = 1; round <= REPEAT; round++ )); do
  start_old_two_panes
  SLOW="$SD/work/slow-session"
  mkdir -p "$SD/work"
  printf '#!/bin/sh\n# %s|END\n[ "$1" = --version ] && sleep %s\nexec %q "$@"\n' \
    "$NEW_BUILD" "$SLOW_PROBE" "$NEW/marspot-session" > "$SLOW"
  chmod +x "$SLOW"
  [[ $(build_of "$SLOW") == "$NEW_BUILD" ]] || die "the slow session build does not name the new build"
  pids=""
  for sid in $SIDS; do pids+=" $(entry "$SD" "$sid" pid)"; done
  execs=$(count "$SD" L3_EXECV_INVOKE)
  swap_core "$SD" "$NEW/marspot-core" "$SLOW"
  check "round $round: both sessions moved to the new build within ${EXEC_WITHIN}s" \
    wait_count "$SD" L3_EXECV_INVOKE "$((execs + 1))" "$EXEC_WITHIN"
  for sid in $SIDS; do
    SID=$sid
    check "round $round: session $sid runs the new build" runs_build "$NEW_BUILD"
  done
  now=""
  for sid in $SIDS; do now+=" $(entry "$SD" "$sid" pid)"; done
  check "round $round: the sessions are the same processes" test "$now" = "$pids"
  for sid in $SIDS; do check "round $round: session $sid is alive" alive "$(entry "$SD" "$sid" pid)"; done
done

echo
if (( panes_failures > 0 )); then
  echo "  (reattach, move and update events of the last phase)"
  grep -E $'\t(L3_REATTACHED|L3_REHOUSE[D]?|L3_REPUBLISHED|L3_EXECV_INVOKE|L3_SWAP_ASK|core\.reattach[a-z_.0-9]*|l3\.republish[a-z_.0-9]*)\t' \
    "$(log_of "$SD")" | cut -c1-220 | sed 's/^/    /'
  echo "FAIL: $panes_failures check(s)"
  exit 1
fi
echo "ALL PASS"
