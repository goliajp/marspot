# shellcheck shell=bash
# The pids of the GUI apps a bench run started, so the run stops exactly
# those: the bench host is shared, and a pattern reaches every process
# whose command line happens to match.  Sourced by the drivers that start
# an app and by the runner that stops it.
#
# A pid file holds one `<pid> <launch-epoch>` line per app started: the
# epoch is taken just before the launch, and a pid only counts while its
# process started at or after it (a pid can be reused).

# app_pid <bundle-id>: the pid LaunchServices has for the app, or nothing.
# Newer macOS prints `pid = 123`, older `"pid"=123`.
app_pid() {
  /usr/bin/lsappinfo info -only pid -app "$1" 2>/dev/null \
    | sed -E -n 's/^(.*[^[:alnum:]])?"?pid"? *= *([0-9]+).*/\2/p' | head -1
}

# started_epoch <pid>: when the process started, in epoch seconds, or
# nothing.  lstart and its parse follow the locale, so both run in C.
started_epoch() {
  local s
  s=$(LC_ALL=C ps -o lstart= -p "$1" 2>/dev/null) || return 0
  [[ -n $s ]] || return 0
  LC_ALL=C date -j -f "%a %b %d %T %Y" "$s" +%s 2>/dev/null || true
}

# record_app_pid <bundle-id> <pid-file> <pid-before> <launch-epoch>:
# append the app's pid, only when nothing of it ran just before the
# launch (<pid-before> empty) and the instance that came up started at or
# after the launch.  An instance someone else runs is never ours to stop.
# The app registers a moment after `open` or the first AppleEvent returns.
record_app_pid() {
  local pid i started
  if [[ -n $3 ]]; then
    echo "record_app_pid: $1 was already running (pid $3) before the launch; not recorded, not stopped" >&2
    return 0
  fi
  for ((i = 0; i < 50; i++)); do
    pid=$(app_pid "$1")
    [[ -z $pid ]] || break
    sleep 0.1
  done
  if [[ -z $pid ]]; then
    echo "record_app_pid: $1 has no pid after 5 s; nothing recorded, so nothing of it will be stopped" >&2
    return 0
  fi
  started=$(started_epoch "$pid")
  if [[ -z $started ]] || (( started < $4 )); then
    echo "record_app_pid: $1 pid $pid started before the launch at $4; not recorded" >&2
    return 0
  fi
  echo "$pid $4" >> "$2"
}

# verified_pid <pid> <launch-epoch> <binary-path>: true while <pid> is
# still that binary and started at or after the launch
verified_pid() {
  local started
  [[ $(ps -o comm= -p "$1" 2>/dev/null) == "$3" ]] || return 1
  started=$(started_epoch "$1")
  [[ -n $started ]] && (( started >= $2 ))
}

# quit_pid <pid>: the app's own quit, addressed to that one process (an
# AppleEvent by app name reaches whichever instance answers to the name)
quit_pid() {
  /usr/bin/osascript -l JavaScript -e "ObjC.import('AppKit');
var a = \$.NSRunningApplication.runningApplicationWithProcessIdentifier($1);
a.isNil() ? 'gone' : (a.terminate() ? 'asked' : 'refused')" 2>/dev/null || true
}

# stop_recorded_pids <pid-file> <binary-path>: stop each recorded pid that
# passes verified_pid, then empty the file.  First the app's own quit, so
# it saves its state like any quit; a quit held by a dialog nobody can
# answer is followed by SIGTERM, then SIGKILL, each after a short grace
# and each re-verified first.
stop_recorded_pids() {
  local p at live=() left sig i
  [[ -s $1 ]] || return 0
  while read -r p at; do
    [[ $p =~ ^[0-9]+$ && $at =~ ^[0-9]+$ ]] || continue
    verified_pid "$p" "$at" "$2" && live+=("$p $at")
  done < <(sort -u "$1")
  : > "$1"
  (( ${#live[@]} )) || return 0
  for p in "${live[@]}"; do quit_pid "${p%% *}" >/dev/null; done
  # the last pass only waits for the SIGKILL to land
  for sig in TERM KILL -; do
    for ((i = 0; i < 20; i++)); do
      left=()
      for p in "${live[@]}"; do verified_pid "${p%% *}" "${p#* }" "$2" && left+=("$p"); done
      (( ${#left[@]} )) || return 0
      sleep 0.1
    done
    if [[ $sig == - ]]; then
      echo "stop_recorded_pids: still there after SIGKILL: ${left[*]}" >&2
      return 0
    fi
    for p in "${left[@]}"; do kill -s "$sig" "${p%% *}" 2>/dev/null || true; done
    live=("${left[@]}")
  done
}
