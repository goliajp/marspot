#!/usr/bin/env bash
# _mixed-versions-checks.sh — checks test-mixed-versions.sh runs in
# more than one phase.  Source it after _sandbox-panes.sh; it uses
# that file's helpers and the suite's SD, SID and count/check.

# Ring the bell in the pane, three times in one line, and go on typing.
# A new session tells its core in a frame of a type an old core does not
# know; the core's reader of the session's stream skips it or rings, and
# either way must not drop the stream.  Keeping the stream shows as no
# new handshake with the session and no reconnect.
bells_keep_the_stream() { # who reads the session's stream
  local hellos reconnects n=$((300 + panes_checks))
  hellos=$(count "$SD" L3_UDS_HELLO)
  reconnects=$(count "$SD" L3_CONTROL_RECONNECTED)
  type_in_pane "$SD" "$SID" "printf '\\a\\a\\a'; echo BELL-\$((1+$n))"
  check "$1: the line after the bells shows" wait_shows "$SD" "$SID" "BELL-$((1 + n))" 10
  type_in_pane "$SD" "$SID" "echo BELL-\$((2+$n))"
  check "$1: the next frame comes after the bells" region_shows "BELL-$((2 + n))"
  check "$1: the session's stream was kept through the bells" \
    test "$(count "$SD" L3_UDS_HELLO) $(count "$SD" L3_CONTROL_RECONNECTED)" = "$hellos $reconnects"
}

# Pane control: a new core tells its shell what it does with typing
# into and watching a pane (`PaneControlCaps`), which an older shell
# skips.  A session that never said it reads those frames is sent none:
# the core answers a request for it `old_session` itself.  The suite
# starts every app with the core's dev seam `MARSPOT_DEV_PANE_CONTROL`,
# under which a new core plays one `PaneType` and one `PaneWatch` for
# its first session pane, as the shell would send them, a few seconds
# after it starts; an older core ignores the variable.

# The core answered a request of type `$1` with code `$2`, within 15 s.
core_answered() { # PaneType | PaneWatch, code
  local i
  for (( i = 0; i < 75; i++ )); do
    grep "$(printf '\tPANE_CONTROL_REPLY\t')" "$(log_of "$SD")" 2>/dev/null |
      grep -F "$(printf '\tmsg_type=%s\t' "$1")" | grep -qF "$(printf '\tcode=%s' "$2")" && return 0
    sleep 0.2
  done
  return 1
}

# The new core over an old app's session, right after the swap: the
# only core that has run the dev seam in this sandbox so far.
pane_control_new_core_old_session() {
  check "the new core told the old shell what it does with pane control" \
    wait_count "$SD" PANE_CONTROL_CAPS 0 15
  check "the old shell still drives the pane after that" frames_keep_coming 1
  local what=(
    "typing into the old session is answered old_session by the core"
    "watching the old session is answered old_session by the core"
    "the old session was sent no pane-control frame"
  )
  # an old session build from after pane control says it reads the
  # frames, and the core forwards both requests to it instead of
  # answering them; decided from the old build itself, never from what
  # the core did, which is what these checks are about
  if old_session_reads_pane_control; then
    local w
    for w in "${what[@]}"; do not_applicable "$w" "the old session build reads pane control"; done
    return
  fi
  check "${what[0]}" core_answered PaneType old_session
  check "${what[1]}" core_answered PaneWatch old_session
  check "${what[2]}" test "$(count "$SD" PANE_CONTROL_FORWARDED)" -eq 0
}

# The old session build sends SessionReadsPaneControl: session 0.11.410
# is the first that does.
old_session_reads_pane_control() {
  local v
  v=$(MARSPOT_NO_REDIRECT=1 "$OLD/marspot-session" --version 2>/dev/null) || return 1
  [[ $v == *.*.* ]] || return 1
  [[ $(printf '%s\n%s\n' 0.11.410 "$v" | sort -V | head -1) == 0.11.410 ]]
}
