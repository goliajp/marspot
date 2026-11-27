# shellcheck shell=bash
# The pids of the GUI apps a bench run started, so the run stops exactly
# those: the bench host is shared, and a pattern reaches every process
# whose command line happens to match.  Sourced by the drivers that start
# an app and by the runner that stops it.

# app_pid <bundle-id>: the pid LaunchServices has for the app, or nothing.
# Newer macOS prints `pid = 123`, older `"pid"=123`.
app_pid() {
  /usr/bin/lsappinfo info -only pid -app "$1" 2>/dev/null \
    | sed -E -n 's/^(.*[^[:alnum:]])?"?pid"? *= *([0-9]+).*/\2/p' | head -1
}

# record_app_pid <bundle-id> <pid-file>: append the app's pid.  The app
# registers a moment after `open` or the first AppleEvent returns.
record_app_pid() {
  local pid i
  for ((i = 0; i < 50; i++)); do
    pid=$(app_pid "$1")
    if [[ -n $pid ]]; then
      echo "$pid" >> "$2"
      return 0
    fi
    sleep 0.1
  done
  echo "record_app_pid: $1 has no pid after 5 s; nothing recorded, so nothing of it will be stopped" >&2
}

# stop_recorded_pids <pid-file> <binary-path> <since-epoch>: kill each
# recorded pid that is still that binary and started at or after <since>
# (a pid can be reused), then empty the file.
stop_recorded_pids() {
  local p started
  [[ -s $1 ]] || return 0
  while read -r p; do
    [[ $(ps -o comm= -p "$p" 2>/dev/null) == "$2" ]] || continue
    started=$(date -j -f "%a %b %d %T %Y" "$(ps -o lstart= -p "$p")" +%s 2>/dev/null) || continue
    (( started >= $3 )) || continue
    kill "$p" 2>/dev/null || true
  done < <(sort -u "$1")
  : > "$1"
}
