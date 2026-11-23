#!/usr/bin/env bash
# _mixed-versions-shadow.sh — the phase of test-mixed-versions.sh in
# which a shadow pane's session goes back to the old session build.
# Source it after _sandbox-panes.sh; it uses that file's helpers and
# the suite's SD, SID, NEW, OLD_BUILD, NEW_BUILD, count/check, die and
# same_pane.

# Wait until `key` in session `sid`'s entry reads `want`.
entry_becomes() { # session id, key, want
  local i
  for (( i = 0; i < 100; i++ )); do
    [[ $(entry "$SD" "$1" "$2") == "$3" ]] && return 0
    sleep 0.2
  done
  echo "    the entry says $2 = '$(entry "$SD" "$1" "$2")'"
  return 1
}

# A session started as the pane's shadow the way the core starts one
# (MARSPOT_SHADOW_OF), in no window.  `current/` still holds the old
# session build, and a SIGTERM from anyone but launchd asks a session
# to move to it.  The old build writes no parent, and the session
# lives on as one that is no shadow.
shadow_goes_back_to_the_old_build() {
  echo "════ rollback: a shadow pane's session goes back to the old build ════"
  local shadow_sid=$((SID + 100)) shadow_pid execs
  (
    export MARSPOT_STATE_DIR="$SD"
    env MARSPOT_SESSION_ID="$shadow_sid" MARSPOT_SHADOW_OF="$SID" \
      nohup "$NEW/marspot-session" >"$SD/shadow.log" 2>&1 < /dev/null &
    dev_record_pid $!
  )
  entry_becomes "$shadow_sid" build "$NEW_BUILD" || die "the shadow's session never registered"
  shadow_pid=$(entry "$SD" "$shadow_sid" pid)
  check "the shadow's first entry names its parent" test "$(entry "$SD" "$shadow_sid" shadow_of)" = "$SID"
  check "the shadow lives beside its parent" alive "$shadow_pid"
  execs=$(count "$SD" L3_EXECV_INVOKE)
  kill -TERM "$shadow_pid"
  wait_count "$SD" L3_EXECV_INVOKE "$execs" 120 || die "the shadow's session never moved to the old build"
  check "the shadow's session runs the old build" entry_becomes "$shadow_sid" build "$OLD_BUILD"
  check "the old build's entry names no parent" test -z "$(entry "$SD" "$shadow_sid" shadow_of)"
  check "the shadow's session is the same process" test "$(entry "$SD" "$shadow_sid" pid)" = "$shadow_pid"
  sleep 1
  check "the shadow's session is alive" alive "$shadow_pid"
  same_pane
  kill -HUP "$shadow_pid" 2>/dev/null
}

# An event `tag` the core logged for session `sid`.
logged_for() { # tag, session id
  grep -qE $'\t'"$1"$'\t.*\tsession='"$2"$'(\t|$)' "$(log_of "$SD")" 2>/dev/null
}
not_logged_for() { ! logged_for "$@"; }

# A shadow pane the new core opens (its dev seam, as the shell's
# PaneSpawn would) is put back in the shadows by a new core that
# restarts, not into the window.  An old core knows no shadow panes: it
# takes the session for one whose window record was lost and shows it
# as a pane of its window, the same session process.
shadow_shows_as_a_pane_under_the_old_core() {
  echo "════ rollback: a shadow pane the new core opened, then the old core ════"
  START_ENV=MARSPOT_DEV_SHADOWS=1 start_app "$NEW"
  local found="" sh pid i
  for (( i = 0; i < 100; i++ )); do
    found=$(grep -l "^shadow_of = $SID\$" "$SD"/sessions/*/entry.toml 2>/dev/null | head -1)
    [[ -n $found ]] && break
    sleep 0.2
  done
  [[ -n $found ]] || die "the new core opened no shadow pane"
  sh=$(basename "$(dirname "$found")")
  pid=$(entry "$SD" "$sh" pid)
  echo "  shadow session $sh pid $pid"
  swap_core "$SD" "$NEW/marspot-core"
  sleep 1
  check "a restarted new core puts the shadow back in the shadows" logged_for SHADOW_ADOPTED "$sh"
  check "and not into the window" not_logged_for core.boot.orphan_adopted "$sh"
  swap_core "$SD" "$OLD/marspot-core"
  sleep 1
  check "the old core shows the shadow as a pane of its window" logged_for core.boot.orphan_adopted "$sh"
  check "the shadow's session is the same process" test "$(entry "$SD" "$sh" pid)" = "$pid"
  check "the shadow's session is alive" alive "$pid"
  same_pane
}
