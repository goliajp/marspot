# shellcheck shell=bash
#
# The Dock probe: a tiny program that registers as a regular app (a Dock
# tile, no window, no menu, never activated), started for a moment to
# read where the Dock puts its tile.  The Dock inserts a new app's tile
# after every tile it holds, and logs the index it inserted it at; the
# probe's index is the number of tiles before it.  Read before and after
# a suite under the same Dock process, a larger index after means the
# suite left a tile -- also one the Dock kept after its app was gone,
# which no app list and no `defaults` key shows.  Needs no accessibility
# access and changes nothing in the Dock.
#
#   dock_probe_build DIR   builds DIR/marspot-dockprobe once; prints its path
#   dock_probe EXE         starts it, reads its insert index from the
#                          Dock's log, stops it (by its pid) and waits
#                          until the Dock logged a tile removal after that;
#                          prints `dock_pid=N` and `index=N`, or
#                          `unread=why` and returns 2
#
# The probe is told from every other app by loginwindow's line naming it
# (its own path) just before the insert, and from a suite's tiles by
# when it runs: alone, before the suite starts and after it was stopped.

DOCK_PROBE_PRED='(process == "Dock" AND category == "dock-tiles") OR (process == "loginwindow" AND eventMessage CONTAINS "Got App URL")'
DOCK_PROBE_HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

dock_probe_build() {
  local dir=$1 exe="$1/marspot-dockprobe"
  [[ -x $exe ]] && { echo "$exe"; return 0; }
  mkdir -p "$dir" || return 1
  cat >"$dir/dockprobe.m" <<'EOF'
#import <AppKit/AppKit.h>
// a regular app for at most argv[1] seconds: a Dock tile, no window;
// SIGTERM ends it at once
int main(int argc, char **argv) {
    double secs = argc > 1 ? atof(argv[1]) : 30;
    @autoreleasepool {
        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyRegular];
        [app finishLaunching];
        NSDate *end = [NSDate dateWithTimeIntervalSinceNow:secs];
        while ([end timeIntervalSinceNow] > 0) {
            NSEvent *e = [app nextEventMatchingMask:NSEventMaskAny untilDate:end inMode:NSDefaultRunLoopMode dequeue:YES];
            if (e) [app sendEvent:e];
        }
    }
    return 0;
}
EOF
  cc -fobjc-arc -o "$exe.tmp" "$dir/dockprobe.m" -framework AppKit >&2 && mv "$exe.tmp" "$exe" || return 1
  echo "$exe"
}

_dock_probe_py() { /usr/bin/python3 "$DOCK_PROBE_HERE/_dock-probe.py" "$@"; }

dock_probe() {
  local exe=$1 dpid dir spid ppid out="" rc from
  dpid="$(/usr/bin/pgrep -x -U "$(id -u)" Dock)" || { echo "unread=no Dock of this login is running"; return 2; }
  dir="$(mktemp -d "${TMPDIR:-/tmp}/dockprobe.XXXXXX")" || { echo "unread=no scratch directory"; return 2; }
  # bounded on its own: it outlives nothing it was started for
  /usr/bin/perl -e 'alarm shift; exec @ARGV' 60 /usr/bin/log stream --style compact \
    --predicate "$DOCK_PROBE_PRED" >"$dir/s" 2>/dev/null &
  spid=$!
  if ! _dock_probe_py ready "$dir/s" 10; then
    out="unread=the Dock's log stream did not start"
  else
    # attached a moment after it says so
    sleep 0.5
    "$exe" 30 >/dev/null 2>&1 &
    ppid=$!
    rc=0
    out="$(_dock_probe_py insert "$dir/s" "$exe" "$dpid" 15)" || rc=$?
    from="$(wc -l <"$dir/s" | tr -d ' ')"
    kill -TERM "$ppid" 2>/dev/null
    wait "$ppid" 2>/dev/null
    case $rc in
      0)
        rc=0
        _dock_probe_py removed "$dir/s" "$from" "$dpid" 15 || rc=$?
        case $rc in
          0) out="dock_pid=$dpid"$'\n'"$(grep '^index=' <<<"$out")" ;;
          3) out="unread=the Dock restarted while the probe's tile went" ;;
          *) out="unread=the Dock logged no tile removal within 15 s of the probe's exit" ;;
        esac ;;
      3) out="unread=the Dock restarted while the probe started" ;;
      *) out="unread=the Dock logged no tile for the probe within 15 s" ;;
    esac
  fi
  kill -TERM "$spid" 2>/dev/null
  wait "$spid" 2>/dev/null
  rm -rf "${dir:?}"
  printf '%s\n' "$out"
  [[ $out == dock_pid=* ]] || return 2
}
