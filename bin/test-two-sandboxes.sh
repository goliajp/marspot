#!/usr/bin/env bash
#
# Two apps on one machine, each with its own state directory, whose
# sessions have the same ids.
#
# Every state directory numbers its sessions from 1, so two sandboxes,
# or a sandbox and an app installed on a new machine, both have a
# session 1.  What a session publishes through shared memory (its
# screen, the text of its grapheme clusters, its window title) must
# stay its own:
#
#   1. while both run, each pane's screen, clusters and title read back
#      as what that pane's own program wrote
#   2. a core that restarts keeps showing its own session
#   3. another app closing its session 1 costs this app nothing: after
#      a core restart the pane is the same process, the same shell and
#      the same screen
#   4. a grid region whose name was removed under a live session is
#      replaced, not answered by ending the session
#
# Run after `cargo build --release`; exits 0 on success.  Every check
# runs even after one failed, so one run shows all of them.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# numbered from 1, like a state directory on a new machine
export MARSPOT_DEV_IDS_FROM_ONE=1
source "$ROOT/bin/_dev-sandbox.sh"
command rm -f "$MARSPOT_STATE_DIR/sessions/.next_id"
source "$ROOT/bin/_sandbox-panes.sh"
SHELL_BIN="$ROOT/target/release/marspot-shell"
CORE_BIN="$ROOT/target/release/marspot-core"
SESSION_BIN="$ROOT/target/release/marspot-session"
PROBE="$ROOT/target/release/examples/shm_probe"
CLI="$SHELL_BIN"

A="$MARSPOT_STATE_DIR"
B="${MARSPOT_STATE_DIR}b"
C="${MARSPOT_STATE_DIR}c"

die() {
  echo "FAIL: $*"
  local sd
  for sd in "$A" "$B" "$C"; do
    echo "  (last 15 events of $sd):"
    tail -15 "$(log_of "$sd")" 2>/dev/null | cut -c1-240 | sed 's/^/    /'
  done
  exit 1
}

cleanup() {
  local sd
  for sd in "$A" "$B" "$C"; do
    "$ROOT/bin/kill-sandbox-marspot.sh" --state-dir "$sd"
  done
  rm -rf "$B" "$C"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

if [[ ! -x "$SHELL_BIN" || ! -x "$CORE_BIN" || ! -x "$SESSION_BIN" ]]; then
  ( cd "$ROOT" && cargo build --release \
      --bin marspot-shell --bin marspot-core --bin marspot-session 2>&1 | tail -3 )
fi
( cd "$ROOT" && cargo build --release --example shm_probe 2>&1 | tail -3 )
[[ -x "$PROBE" ]] || die "the probe did not build"

title() { "$PROBE" title "$1/sessions/$2" 2>/dev/null; }
# the published frame itself; `screen` may answer from the record
frame_shows() {
  local out
  out=$("$PROBE" screen "$1/sessions/$2" 2>/dev/null)
  [[ $out == *"$3"* ]]
}

cleanup
FLAG_A=$'\360\237\207\257\360\237\207\265'
FLAG_B=$'\360\237\207\253\360\237\207\267'

echo "════ two apps, the same session id ════"
launch "$A" "$SHELL_BIN"
WORK="$A/two-sandboxes"
mkdir -p "$WORK"
printf '\033]2;TITLE-A\007MARK-A \360\237\207\257\360\237\207\265\n' > "$WORK/a.txt"
printf '\033]2;TITLE-B\007MARK-B \360\237\207\253\360\237\207\267\n' > "$WORK/b.txt"
SID=$(first_pane "$A") || die "$A: no pane"
run_in_pane "$A" "$SID" "$WORK/a.txt"
wait_shows "$A" "$SID" MARK-A 10 || die "$A: the pane never showed what its program printed"
A_PID=$(entry "$A" "$SID" pid)
A_SHELL=$(entry "$A" "$SID" shell_child_pid)
echo "  A: session $SID pid $A_PID shell $A_SHELL region $(entry "$A" "$SID" shm_name)"

launch "$B" "$SHELL_BIN"
SID_B=$(first_pane "$B") || die "$B: no pane"
[[ $SID_B == "$SID" ]] || die "the two state directories did not give their panes the same id ($SID, $SID_B)"
run_in_pane "$B" "$SID" "$WORK/b.txt"
wait_shows "$B" "$SID" MARK-B 10 || die "$B: the pane never showed what its program printed"
B_PID=$(entry "$B" "$SID" pid)
B_SHELL=$(entry "$B" "$SID" shell_child_pid)
echo "  B: session $SID pid $B_PID shell $B_SHELL region $(entry "$B" "$SID" shm_name)"
sleep 1

both_are_themselves() {
  check "A's frame is A's" frame_shows "$A" "$SID" MARK-A
  check "A's screen is not B's" lacks "$A" "$SID" MARK-B
  check "A's cluster text is A's" frame_shows "$A" "$SID" "$FLAG_A"
  check "A's title is A's" test "$(title "$A" "$SID")" = TITLE-A
  check "B's frame is B's" frame_shows "$B" "$SID" MARK-B
  check "B's screen is not A's" lacks "$B" "$SID" MARK-A
  check "B's cluster text is B's" frame_shows "$B" "$SID" "$FLAG_B"
  check "B's title is B's" test "$(title "$B" "$SID")" = TITLE-B
}
same_processes() { # label, state dir, session pid, shell pid
  check "$1's session is the same process" test "$(entry "$2" "$SID" pid)" = "$3"
  check "$1's session process is alive" alive "$3"
  check "$1's shell is alive" alive "$4"
}
both_are_themselves

echo "════ A's core restarts while B runs ════"
swap_core "$A" "$CORE_BIN"
sleep 1
same_processes A "$A" "$A_PID" "$A_SHELL"
check "A's core took its session back" test "$(count "$A" L3_REATTACHED)" -ge 1
both_are_themselves

echo "════ a third app opens session $SID and closes it ════"
launch "$C" "$SHELL_BIN" MARSPOT_DEV_CLOSE_PANES=1
wait_count "$C" DEV_CLOSE_PANE 0 60 || die "$C: the pane was never closed"
sleep 3
swap_core "$A" "$CORE_BIN"
swap_core "$B" "$CORE_BIN"
sleep 1
same_processes A "$A" "$A_PID" "$A_SHELL"
same_processes B "$B" "$B_PID" "$B_SHELL"
both_are_themselves

echo "════ the name of A's grid region is removed under it ════"
if alive "$A_PID"; then
  "$PROBE" unlink "$A/sessions/$SID"
  swap_core "$A" "$CORE_BIN"
  wait_count "$A" L3_REHOUSED 0 20
  same_processes A "$A" "$A_PID" "$A_SHELL"
  check "A's session moved to a region made for it" test "$(count "$A" L3_REHOUSED)" -ge 1
  check "A's core did not end the session" test "$(grep -c 'core.reattach.l3_failed' "$(log_of "$A")")" -eq 0
  both_are_themselves
else
  check "A's session lived to this phase" false
fi

echo
if (( panes_failures > 0 )); then
  for sd in "$A" "$B"; do
    echo "  ($sd: reattach and spawn events)"
    grep -E $'\t(L3_REATTACHED|L3_SPAWN|L3_REHOUSE[D]?|core\.reattach[a-z_.0-9]*)\t' "$(log_of "$sd")" \
      | cut -c1-220 | sed 's/^/    /'
  done
  echo "FAIL: $panes_failures check(s)"
  exit 1
fi
echo "ALL PASS"
