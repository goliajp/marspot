# shellcheck shell=bash
# shellcheck disable=SC2034 # its variables are for the tests that source it
#
# The inputs and the verdict of bin/_e2e-suite-clean.sh's tests
# (test-e2e-suite-clean.sh, test-e2e-suite-clean-dock.sh): sourced after
# ROOT, T (a scratch directory) and failed=0 are set.  The log lines are
# in the formats the Dock, loginwindow and tccd print on macOS 26 and 27
# (paths not ours replaced).

DOCK='1) "Dock" ASN:0x0-0x1001:
    bundle path="/System/Library/CoreServices/Dock.app"
    executable path="/System/Library/CoreServices/Dock.app/Contents/MacOS/Dock"
    pid = 726 token=[sess=1] type="UIElement" flavor=4
2) "Finder" ASN:0x0-0x1002:
    bundle path="/System/Library/CoreServices/Finder.app"
    executable path="/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder"
    bundleID="com.apple.finder"
    pid = 729 token=[sess=1] type="Foreground" flavor=3'
MARSPOT='3) "Marspot" ASN:0x0-0x1003:
    bundle path="/tmp/msp-e2e-1-1-lander/Marspot.app"
    executable path="/tmp/msp-e2e-1-1-lander/Marspot.app/Contents/MacOS/marspot-shell"
    bundleID=[ NULL ]
    pid = 4242 token=[sess=1] type="BackgroundOnly" flavor=3'
MARSPOT_PRIVATE="${MARSPOT//"/tmp/"/"/private/tmp/"}"
HEADER='Timestamp              Ty Process[PID:TID]'
TILE='Df Dock[726:1d82] [com.apple.dock:dock-tiles]'
launch() { echo "2026-11-27 10:00:$1 $TILE Inserting tile: <private> at index: ${2:-37} for reason: -[DockBar _handleLaunchNotification:data:]"; }
# app URL, seconds: loginwindow naming the app the Dock inserts a tile for next
opened() { echo "2026-11-27 10:00:$2 Df loginwindow[179:9d5] [com.apple.loginwindow.logging:Standard] -[Application _updateInformationInternal] | Got App URL: file://$1/"; }
# the Dock probe's answer as bin/_dock-probe.sh prints it: index [dock pid]
probed() { printf 'dock_pid=%s\nindex=%s\n' "${2:-726}" "$1"; }
APP_URL=/private/tmp/msp-e2e-1-1-x/MarspotOwnershipTest.app
PROBE_URL=/private/tmp/msp-e2e-1-1-x/ownprobe
DOCKPROBE_URL=/private/tmp/msp-e2e-1-dockprobe/marspot-dockprobe
remove() { echo "2026-11-27 10:00:$1 $TILE Removing tiles identical to: <private> for reason: ${2:-_emptyDeleteList()}"; }
# the Dock probe's own lines in second SS, as a run logs them before and after the suite:
# named, inserted at INDEX, its tile removed after it was stopped
probe_tile() { opened "$DOCKPROBE_URL" "$1.000"; launch "$1.004" "${2:-37}"; remove "$1.500"; }
TILE_REMOVED='-[DockBar handleDockEventTileRemoved:]'
ndjson() { python3 -c 'import json,sys; [print(json.dumps({"eventMessage": m})) for m in sys.argv[1:]]' "$@"; }
attribution() { # msgID, responsible path, accessing path
  echo "AUTHREQ_ATTRIBUTION: msgID=$1, attribution={responsible={TCCDProcess: identifier=x, pid=1708, auid=501, euid=501, responsible_path=$2, binary_path=$2}, accessing={TCCDProcess: identifier=y, pid=70053, auid=501, euid=501, binary_path=$3}, requesting={TCCDProcess: identifier=com.apple.WindowServer, pid=422, auid=88, euid=88, binary_path=/System/Library/PrivateFrameworks/SkyLight.framework/Versions/A/Resources/WindowServer}, },"
}
ctx() { echo "AUTHREQ_CTX: msgID=$1, function=<private>, service=$2, preflight=yes, query=1, client_dict=(null), daemon_dict=<private>"; }
OTHER=/Applications/Other.app/Contents/MacOS/Other
BUILD=/Users/u/work/e2e-marspot/target/release/marspot-shell
LANDER=/private/tmp/msp-e2e-1-1-lander/Marspot.app/Contents/MacOS/marspot-shell
TCC_OTHER="$(ndjson "$(ctx 9.1 kTCCServiceCamera)" "$(attribution 9.1 "$OTHER" "$OTHER")")"
TCC_OURS="$(ndjson "$(ctx 401.7 kTCCServiceAppleEvents)" "$(attribution 401.7 "$BUILD" "$OTHER")")"
result() { echo "AUTHREQ_RESULT: msgID=$1, authValue=1, authReason=5, authVersion=1, desired_auth=0, error=(null),"; }
TCC_LISTEN="$(ndjson "$(ctx 403.1 kTCCServiceListenEvent)" "$(attribution 403.1 "$OTHER" "$BUILD")" "$(result 403.1)")"
TCC_LISTEN_PROMPT="$(ndjson "$(attribution 404.1 "$OTHER" "$BUILD")" "$(ctx 404.1 kTCCServiceListenEvent)" \
  "AUTHREQ_PROMPTING: msgID=404.1, service=kTCCServiceListenEvent" "$(result 404.1)")"
TCC_LISTEN_ASK="$(ndjson "AUTHREQ_CTX: msgID=405.1, function=<private>, service=kTCCServiceListenEvent, preflight=no, query=1," \
  "$(attribution 405.1 "$OTHER" "$BUILD")" "$(result 405.1)")"
TCC_ACCESSING="$(ndjson "$(ctx 402.1 kTCCServiceListenEvent)" "$(attribution 402.1 "$OTHER" "$LANDER")")"
ME=$'4000\theavy\tholding since 2026-11-27 10:00:00\tsh -c stopped=; ...'
SOMEONE=$'5000\theavy\tholding since 2026-11-27 09:59:00\tcargo build'
# tile LABEL [PATH [BUNDLE]]: one Dock tile as `defaults read com.apple.dock` prints it
tile() {
  printf '    {\n        GUID = 1;\n        "tile-data" =         {\n            "bundle-identifier" = "%s";\n' "${3:-com.x.$1}"
  printf '            "file-data" =             {\n                "_CFURLString" = "file://%s/";\n            };\n' "${2:-/Applications/$1.app}"
  printf '            "file-label" = %s;\n        };\n        "tile-type" = "file-tile";\n    },\n' "$1"
}
# each argument is one tile's words, split on purpose
# shellcheck disable=SC2086
tiles() { echo "("; local t; for t in "$@"; do tile $t; done; echo ")"; }
# the before snapshot's list of the tiles in /Applications named
entries() { local o="" l; for l; do o+="${o:+, }[\"/Applications/$l.app/\", \"com.x.$l\", \"$l\"]"; done; echo "[$o]"; }
OURS_TILE="Marspot /private/tmp/msp-e2e-1-1-lander/Marspot.app com.goliajp.marspot"

# $1 name; fills $T/$1 with the inputs of a clean suite run alone
base() {
  local d="$T/$1"
  mkdir -p "$d/in"
  printf 'start=2026-11-27 10:00:00\nstart_epoch=1700000000\nfg=1\npersistent=2\nrecent=1\nothers_snap=\ndock_live=1\ntcc_live=1\n' >"$d/before"
  echo 'fg_set=["com.apple.finder"]' >>"$d/before"
  echo "persistent_entries=$(entries A B)" >>"$d/before"
  echo "recent_entries=$(entries C)" >>"$d/before"
  # the probe put its tile at 37 before the suite and puts it there after it
  printf 'probe_dock_pid=726\nprobe_index=37\n' >>"$d/before"
  probed 37 >"$d/in/probe-end.txt"
  printf '%s\n' "$DOCK" >"$d/in/lsappinfo.txt"
  tiles A B >"$d/in/persistent.txt"
  tiles C >"$d/in/recent.txt"
  { echo "$HEADER"; opened "$APP_URL" 00.996; launch 01.000; remove 05.000; } >"$d/in/dock.log"
  printf '%s\n{"count":2,"finished":1}\n' "$TCC_OTHER" >"$d/in/tcc.ndjson"
  printf '%s\n' "$ME" >"$d/in/status.txt"
  # our own take, logged a second before the snap
  printf '1699999999\theavy\t0\ttook\tsh -c stopped=; ...\t/w\n' >"$d/in/lock.log"
}

# $1 name, $2 expected exit, then lines the result must have, then -- and
# the extra arguments of the check
expect() {
  local name=$1 want=$2 d="$T/$1" got=0 ok=1 line
  shift 2
  local lines=()
  while [[ $# -gt 0 && $1 != -- ]]; do lines+=("$1"); shift; done
  [[ $# -gt 0 ]] && shift
  E2E_CLEAN_INPUTS="$d/in" "$ROOT/bin/_e2e-suite-clean.sh" check "$d/before" "$d/result" \
    --suite-rc 0 --lock-pid 4000 "$@" /tmp/msp-e2e-1- >"$d/out" 2>&1 || got=$?
  [[ $got == "$want" && $(tail -1 "$d/result") == "clean=$want" && $(head -1 "$d/result") == suite_rc=0 ]] || ok=0
  for line in "${lines[@]}"; do grep -qF -- "$line" "$d/result" || ok=0; done
  if ((ok)); then
    echo "  ok    $name (clean=$got)"
  else
    echo "  FAIL  $name: exit $got, wanted $want with: ${lines[*]}"
    sed 's/^/        /' "$d/out"
    failed=1
  fi
}
