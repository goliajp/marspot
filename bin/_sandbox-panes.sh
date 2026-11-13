#!/usr/bin/env bash
# _sandbox-panes.sh — sourced by suites that run more than one sandbox
# app, or more than one build of the app, and look at what a pane's
# session publishes.  Source it after _dev-sandbox.sh.
#
# Every function takes the state directory it acts on, so one script
# can drive several sandboxes; nothing here signals a process (stopping
# them is dev_kill_shell_core's job, through kill-sandbox-marspot.sh).
#
# The caller sets CLI to the marspot-shell binary used for --panes,
# --send, --read and --trigger, and defines `die`.

panes_failures=0
check() { # description, command…
  local what=$1
  shift
  if "$@"; then
    echo "  ok    $what"
  else
    echo "  FAIL  $what"
    panes_failures=$((panes_failures + 1))
  fi
}

in_sd() { # state dir, command…
  local sd=$1
  shift
  MARSPOT_STATE_DIR="$sd" "$@"
}
log_of() { echo "$1/logs/marspot.log"; }
count() { # state dir, event tag
  local n
  n=$(grep -c "$(printf '\t%s\t' "$2")" "$(log_of "$1")" 2>/dev/null)
  echo "${n:-0}"
}
logged() { # state dir, text
  grep -qF -- "$2" "$(log_of "$1")" 2>/dev/null
}
wait_count() { # state dir, event tag, more than, seconds
  local i
  for (( i = 0; i < $4 * 5; i++ )); do
    (( $(count "$1" "$2") > $3 )) && return 0
    sleep 0.2
  done
  return 1
}
entry() { # state dir, session id, key
  sed -n "s/^$3 = \"\{0,1\}\([^\"]*\)\"\{0,1\}\$/\1/p" "$1/sessions/$2/entry.toml" 2>/dev/null
}
alive() { kill -0 "$1" 2>/dev/null; }
screen() { in_sd "$1" "$CLI" --read "$2" 2>/dev/null; }
# whole output first: under pipefail a reader that stops at the first
# match fails the writer
shows() {
  local out
  out=$(screen "$1" "$2")
  [[ $out == *"$3"* ]]
}
lacks() { ! shows "$@"; }
wait_shows() { # state dir, session id, text, seconds
  local i
  for (( i = 0; i < $4 * 5; i++ )); do
    shows "$1" "$2" "$3" && return 0
    sleep 0.2
  done
  return 1
}

launch() { # state dir, shell binary, extra env…
  local sd=$1 shell=$2
  shift 2
  rm -rf "$sd"
  mkdir -p "$sd/logs"
  (
    export MARSPOT_STATE_DIR="$sd"
    # a core that was swapped in runs from binaries/current, where a
    # sandbox has no session binary beside it to start a pane from
    env MARSPOT_SESSION_BIN="$(dirname "$shell")/marspot-session" "$@" \
      nohup "$shell" >"$sd/run.log" 2>&1 < /dev/null &
    dev_record_pid $!
  )
  wait_count "$sd" HELLO_ACK 0 90 || die "$sd: the core never said hello"
}

first_pane() { # state dir
  local i sid
  for (( i = 0; i < 150; i++ )); do
    sid=$(in_sd "$1" "$CLI" --panes 2>/dev/null | awk 'NR>1 {print $1; exit}')
    [[ -n $sid ]] && { echo "$sid"; return 0; }
    sleep 0.4
  done
  return 1
}

# Wait until the pane's shell runs what is typed into it.
pane_ready() { # state dir, session id
  local sd=$1 sid=$2 mark="$1/ready-$2" attempt i
  for (( attempt = 1; attempt <= 10; attempt++ )); do
    in_sd "$sd" "$CLI" --send "$sid" "echo reading > $mark" >/dev/null 2>&1 || { sleep 0.5; continue; }
    for (( i = 0; i < 15; i++ )); do
      [[ -f $mark ]] && return 0
      sleep 0.2
    done
  done
  die "$sd: pane $sid's shell never ran anything typed into it"
}

type_in_pane() { # state dir, session id, text: retried only while refused, which queues nothing
  local i
  for (( i = 0; i < 40; i++ )); do
    in_sd "$1" "$CLI" --send "$2" "$3" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  die "$1: --send to pane $2 was refused 40 times"
}

# Print a file in the pane and keep a program in the foreground after
# it, so no prompt redraws the window title the file set.
run_in_pane() { # state dir, session id, file to print
  pane_ready "$1" "$2"
  type_in_pane "$1" "$2" "cat $3; sleep 3000"
}

# Replace the running core through the update path: stage `core`, and
# `session` too when given, and ask the shell to take them.
swap_core() { # state dir, core binary, [session binary]
  local sd=$1 stable old new i
  stable=$(count "$sd" UPDATE_STABLE)
  old=$(in_sd "$sd" dev_core_pids | head -1)
  mkdir -p "$sd/binaries/pending"
  cp "$2" "$sd/binaries/pending/marspot-core"
  if [[ -n ${3:-} ]]; then cp "$3" "$sd/binaries/pending/marspot-session"; fi
  in_sd "$sd" "$CLI" --trigger >/dev/null
  wait_count "$sd" UPDATE_STABLE "$stable" 300 || die "$sd: the core swap never settled"
  for (( i = 0; i < 50; i++ )); do
    new=$(in_sd "$sd" dev_core_pids | head -1)
    [[ -n $new && $new != "$old" ]] && return 0
    sleep 0.2
  done
  die "$sd: the core is still pid $old after the swap"
}
