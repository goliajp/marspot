#!/usr/bin/env bash
# bin/test-e2e-suite-clean.sh — the decision of bin/_e2e-suite-clean.sh
# over saved inputs: what a runner shows after a clean suite, and after
# each kind of leftover or unreadable source.  Local, a second or two;
# reads no live log and launches nothing.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$(mktemp -d /tmp/marspot-suite-clean.XXXXXX)"
trap 'rm -rf "${T:?}"' EXIT
failed=0

DOCK='1) "Dock" ASN:0x0-0x1001:
    bundle path="/System/Library/CoreServices/Dock.app"
    executable path="/System/Library/CoreServices/Dock.app/Contents/MacOS/Dock"
    pid = 726 token=[sess=1] type="UIElement" flavor=4
2) "Finder" ASN:0x0-0x1002:
    bundle path="/System/Library/CoreServices/Finder.app"
    executable path="/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder"
    pid = 729 token=[sess=1] type="Foreground" flavor=3'
MARSPOT='3) "Marspot" ASN:0x0-0x1003:
    bundle path="/tmp/msp-e2e-1-1-lander/Marspot.app"
    executable path="/tmp/msp-e2e-1-1-lander/Marspot.app/Contents/MacOS/marspot-shell"
    pid = 4242 token=[sess=1] type="BackgroundOnly" flavor=3'
LAUNCH='2026-11-27 10:00:01.000 Df Dock[726:1] [com.apple.dock:dock-tiles] Inserting tile: <private> at index: 9 for reason: -[DockBar _handleLaunchNotification:data:]'
REMOVE='2026-11-27 10:00:05.000 Df Dock[726:1] [com.apple.dock:dock-tiles] Removing tiles identical to: <private> for reason: -[DockBar handleDockEventTileRemoved:]'
TCC_OURS='{"eventMessage":"AUTHREQ_ATTRIBUTION: msgID=401.7, attribution={responsible={TCCDProcess: identifier=marspot, responsible_path=/Users/u/work/e2e-marspot/target/release/marspot-shell, binary_path=/Users/u/work/e2e-marspot/target/release/marspot-shell}, requesting={TCCDProcess: binary_path=/Users/u/work/e2e-marspot/target/release/marspot-core}, },"}
{"eventMessage":"AUTHREQ_CTX: msgID=401.7, function=TCCAccessRequest, service=kTCCServiceAppleEvents, preflight=no"}'
TCC_OTHER='{"eventMessage":"AUTHREQ_CTX: msgID=9.1, service=kTCCServiceCamera, binary_path=/Applications/Simulator.app/Contents/MacOS/Simulator"}'

# $1 name; fills $T/$1 with the inputs of a clean suite
base() {
  local d="$T/$1"
  mkdir -p "$d/in"
  printf 'start=2026-11-27 10:00:00\nfg=1\npersistent=2\nrecent=1\n' >"$d/before"
  printf '%s\n' "$DOCK" >"$d/in/lsappinfo.txt"
  printf '(\n{ "file-label" = A; },\n{ "file-label" = B; }\n)\n' >"$d/in/persistent.txt"
  printf '(\n{ "file-label" = C; }\n)\n' >"$d/in/recent.txt"
  printf 'Timestamp               Ty Process[PID:TID]\n%s\n%s\n' "$LAUNCH" "$REMOVE" >"$d/in/dock.log"
  printf '%s\n{"count":1,"finished":1}\n' "$TCC_OTHER" >"$d/in/tcc.ndjson"
}

# $1 name, $2 expected exit, $3 a line the result must have; then the
# extra arguments of the check
expect() {
  local name=$1 want=$2 line=$3 d="$T/$1" got=0
  shift 3
  E2E_CLEAN_INPUTS="$d/in" "$ROOT/bin/_e2e-suite-clean.sh" check "$d/before" "$d/result" \
    --suite-rc 0 "$@" /tmp/msp-e2e-1- >"$d/out" 2>&1 || got=$?
  if [[ $got == "$want" ]] && grep -qF -- "$line" "$d/result" && [[ $(tail -1 "$d/result") == "clean=$want" ]]; then
    echo "  ok    $name (clean=$got)"
  else
    echo "  FAIL  $name: exit $got, wanted $want with a line '$line'"
    sed 's/^/        /' "$d/out"
    failed=1
  fi
}

base clean
expect clean 0 "dock: inserts=1 removals=1 other_removals=0"

base insert
printf '%s\n' "$LAUNCH" >>"$T/insert/in/dock.log"
expect insert 1 "left: dock: 2 launch tile insert(s), 1 removal(s)"

base persistent
printf '(\n{ "file-label" = A; }\n)\n' >"$T/persistent/in/persistent.txt"
expect persistent 1 "left: persistent count changed: 2 before, 1 after"

base record
printf '%s\n%s\n' "$DOCK" "$MARSPOT" >"$T/record/in/lsappinfo.txt"
expect record 1 "left: record: BackgroundOnly pid=4242 /tmp/msp-e2e-1-1-lander/Marspot.app"

base known
printf '%s\n%s\n' "$DOCK" "$MARSPOT" >"$T/known/in/lsappinfo.txt"
expect known 0 "clean=0" --known "4242:/tmp/msp-e2e-1-1-lander/Marspot.app"

base foreground
printf '%s\n%s\n' "$DOCK" "${MARSPOT//BackgroundOnly/Foreground}" >"$T/foreground/in/lsappinfo.txt"
expect foreground 1 "left: fg count changed: 1 before, 2 after"

base tcc
printf '%s\n%s\n{"count":3,"finished":1}\n' "$TCC_OTHER" "$TCC_OURS" >"$T/tcc/in/tcc.ndjson"
expect tcc 1 "left: tcc: kTCCServiceAppleEvents req=/Users/u/work/e2e-marspot/target/release/marspot-shell"

base unread-log
rm "$T/unread-log/in/dock.log"
expect unread-log 2 "unread: dock log: log show gave no output"

base unread-ls
printf 'nothing\n' >"$T/unread-ls/in/lsappinfo.txt"
expect unread-ls 2 "unread: lsappinfo: no list of the console login (no Dock in it)"

base left-wins
rm "$T/left-wins/in/tcc.ndjson"
printf '%s\n' "$LAUNCH" >>"$T/left-wins/in/dock.log"
expect left-wins 1 "unread: tcc log: log show did not finish"

((failed)) && exit 1
echo "PASS: test-e2e-suite-clean"
