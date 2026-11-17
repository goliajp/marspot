#!/usr/bin/env bash
# bin/test-release-update.sh — a copy installed from an older disk image
# takes a release that was built for publishing, without it being
# published.
#
#   bin/publish-release.sh --rehearse build/rehearsal     # the newer version
#   bin/test-release-update.sh <older Marspot.dmg> build/rehearsal
#
# The sandboxed update suites prove the mechanism on binaries built for
# testing.  This is the thing itself: the notarised app out of the disk
# image a person was handed, an empty HOME, and the tarball and
# signature exactly as bin/publish-release.sh would upload them, served
# from the runner in place of the releases API.  The copy has to find
# it, verify it against the key compiled into it, and come back up on
# it with its pane still there.
set -euo pipefail

DMG="${1:?usage: $0 <older Marspot.dmg> <rehearsal dir>}"
REL="${2:?usage: $0 <older Marspot.dmg> <rehearsal dir>}"
HOST="${HOST:-mini}"
GUI_UID="${GUI_UID:-501}"
RUN_ID="relupd-$$-$(date +%s)"
REMOTE="/tmp/marspot-$RUN_ID"
TAR="marspot-aarch64-apple-darwin.tar.gz"

for f in "$REL/$TAR" "$REL/$TAR.sig" "$REL/feed.json"; do
  [[ -f "$f" ]] || { echo "missing $f — run bin/publish-release.sh --rehearse $REL" >&2; exit 1; }
done
TAG="$(sed -n 's/.*"tag_name": *"\(.*\)".*/\1/p' "$REL/feed.json")"

ssh "$HOST" "mkdir -p $REMOTE/serve"
scp -q "$DMG" "$HOST:$REMOTE/old.dmg"
scp -q "$REL/$TAR" "$REL/$TAR.sig" "$HOST:$REMOTE/serve/"

ssh "$HOST" "sudo -n launchctl asuser $GUI_UID sudo -u \$(id -un $GUI_UID) /usr/local/bin/bench-lock heavy --max-wait 1800 /bin/bash -s -- $REMOTE $TAG $TAR" <<'REMOTE_SCRIPT'
set -uo pipefail
REMOTE="$1"; TAG="$2"; TAR="$3"
FRESH="$REMOTE/home"
STATE="$FRESH/Library/Application Support/marspot"
LOG="$FRESH/Library/Logs/Marspot/marspot.log"
TREE="$STATE/binaries"
fail=0
check() { if "${@:2}"; then echo "  ok    $1"; else echo "  FAIL  $1"; fail=1; fi; }
ours() { ps -axww -E -o pid=,command= | grep -F "HOME=$FRESH" | grep -v grep; }
# pid and executable path, tab-separated: the state directory's path has
# a space in it, so a command line cannot be split on blanks
procs() { for p in $(ours | awk '{print $1}'); do printf '%s\t%s\n' "$p" "$(ps -o comm= -p "$p")"; done; }

cleanup() {
  [[ -n "${HTTP_PID:-}" ]] && kill "$HTTP_PID" 2>/dev/null
  for sig in TERM KILL; do
    for p in $(ours | awk '{print $1}'); do kill -$sig "$p" 2>/dev/null; done
    sleep 2
  done
  rm -rf "${REMOTE:?}"
}
trap cleanup EXIT

mkdir -p "$FRESH/tmp" "$FRESH/Applications" "$REMOTE/mnt"
hdiutil attach "$REMOTE/old.dmg" -nobrowse -readonly -mountpoint "$REMOTE/mnt" -quiet || { echo "cannot mount"; exit 1; }
cp -R "$REMOTE/mnt/Marspot.app" "$FRESH/Applications/"
hdiutil detach "$REMOTE/mnt" -quiet
APP="$FRESH/Applications/Marspot.app"
OLD="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")"
echo "==> installed $OLD, release on offer $TAG"

# the releases API, as far as the updater reads it
PORT=$(( 20000 + RANDOM % 20000 ))
cat > "$REMOTE/serve/feed.json" <<JSON
{"tag_name": "$TAG", "assets": [
 {"name": "$TAR", "browser_download_url": "http://127.0.0.1:$PORT/$TAR"},
 {"name": "$TAR.sig", "browser_download_url": "http://127.0.0.1:$PORT/$TAR.sig"}]}
JSON
python3 -m http.server "$PORT" --bind 127.0.0.1 -d "$REMOTE/serve" >/dev/null 2>&1 &
HTTP_PID=$!

# no agent installs here: this run is about the update alone
mkdir -p "$STATE"
printf 'agents.install = false\n' > "$STATE/settings.toml"

env -i HOME="$FRESH" USER="$(id -un)" LOGNAME="$(id -un)" SHELL=/bin/zsh \
  PATH=/usr/bin:/bin:/usr/sbin:/sbin TMPDIR="$FRESH/tmp" LANG=en_US.UTF-8 \
  MARSPOT_UPDATE_FEED="http://127.0.0.1:$PORT/feed.json" \
  "$APP/Contents/MacOS/marspot-shell" >"$FRESH/stdout.log" 2>&1 &
SHELL_PID=$!

wait_for() { local until=$(( $(date +%s) + $1 )); shift; until "$@"; do [ "$(date +%s)" -ge "$until" ] && return 1; sleep 1; done; }
booted() { grep -q CORE_BOOT "$LOG" 2>/dev/null && [ "$(ls "$STATE/sessions" 2>/dev/null | wc -l)" -ge 1 ]; }
staged() { [ -f "$TREE/pending/marspot-core" ] && [ -f "$TREE/pending/marspot-shell" ] && [ -f "$TREE/pending/marspot-session" ]; }
starts() { grep -c $'\tSTARTUP\tmarspot-shell starting' "$LOG" 2>/dev/null; }
restarted() { [ "$(starts)" -ge 2 ] && [ "$(grep -c CORE_BOOT "$LOG")" -ge 2 ] && ! [ -e "$TREE/pending/marspot-core" ]; }

check "the installed copy boots" wait_for 60 booted
PANE_BEFORE="$(ls "$STATE/sessions")"
SESSION_BEFORE="$(procs | awk -F'\t' '$2 ~ /\/marspot-session$/ {print $1}' | head -1)"
check "it finds, verifies and stages the release (30 s after start)" wait_for 150 staged
# what leaving the window does; nobody is at this console to do it
env -i HOME="$FRESH" PATH=/usr/bin:/bin "$APP/Contents/MacOS/marspot-shell" --trigger >/dev/null 2>&1
check "it comes back up on the release" wait_for 300 restarted

# read whole before matching: under pipefail, grep -q quitting early
# hands codesign a SIGPIPE and fails the pipe on a match
sig() { codesign -dv "$1" 2>&1; }
team() { [[ "$(sig "$1")" == *TeamIdentifier=KF79DRC524* ]]; }
runtime() { [[ "$(sig "$1")" =~ flags=[^[:space:]]*runtime ]]; }
from_state() { [[ "$(procs | awk -F'\t' -v s="/$1" 'substr($2, length($2) - length(s) + 1) == s {print $2}')" == */binaries/* ]]; }
same_pane() { [ "$(ls "$STATE/sessions")" == "$PANE_BEFORE" ] && kill -0 "$SESSION_BEFORE" 2>/dev/null; }
no_panic() { ! grep -qi 'panic' "$LOG" "$FRESH/stdout.log"; }
no_errors() { [[ -z "$(awk -F'\t' '$3=="ERROR"' "$LOG")" ]]; }

echo "==> what is running"
procs | awk -F'\t' '{print "  " $1 "  " $2}' | sed "s|$FRESH|~|"
check "shell process is still the one that was started" kill -0 $SHELL_PID
check "core runs from the state directory" from_state marspot-core
for b in marspot-shell marspot-core marspot-session; do
  check "$b in place carries the Developer ID" team "$TREE/current/$b"
  check "$b in place has the Hardened Runtime" runtime "$TREE/current/$b"
done
echo "==> what is in place"
ls -la "$TREE/current" 2>&1 | sed 's/^/  /'
for b in marspot-shell marspot-core marspot-session; do
  echo "  $b: $(codesign -dv "$TREE/current/$b" 2>&1 | grep -E '^(Identifier|TeamIdentifier|CodeDirectory)' | tr '\n' ' ')"
done
check "the pane and its session process survived" same_pane
check "no panic" no_panic
check "no ERROR lines" no_errors
echo "==> the update, as the log tells it"
grep -E $'\t(STARTUP|CORE_BOOT|UPDATE_[A-Z_]*|SHELL_SELF_UPDATE|PROBE_[A-Z_]*)\t' "$LOG" | awk -F'\t' '{print "  " $1 "  " $4 "  " $7 "  " substr($8,1,70)}' | head -24
echo "==> warnings"
awk -F'\t' '$3=="WARN"{print "  " $7 "  " substr($8,1,140)}' "$LOG" | sort | uniq -c | sort -rn | head
echo "==> stdout/stderr (tail)"
tail -4 "$FRESH/stdout.log" | sed 's/^/  /'
exit $fail
REMOTE_SCRIPT
