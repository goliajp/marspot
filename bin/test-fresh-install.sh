#!/usr/bin/env bash
# bin/test-fresh-install.sh — the disk image, on a Mac, as someone who
# has never had marspot or any agent CLI.
#
#   bin/test-fresh-install.sh build/dist/Marspot-0.4.0.dmg     # host: mini
#   HOST=other-mini bin/test-fresh-install.sh <dmg>
#
# What the other suites cannot see is the machine that has nothing: no
# ~/.local/bin, no ~/.claude, no usage feed, no helper on PATH.  This
# copies the app out of the image the way a person would, starts it in
# the console session with an empty HOME and nothing of marspot's in
# the environment, and checks what it is supposed to do unasked:
# install the three CLIs, keep its state where the docs say, put
# ~/.local/bin on the pane's PATH, and write its hooks and skill into
# the config dirs the installers create.
#
# It reaches the network (three vendor downloads) and takes a few
# minutes.  Everything it starts carries the scratch HOME in its
# environment, and that is how the cleanup finds it: nothing else on
# the host is touched.
set -euo pipefail

DMG="${1:?usage: $0 <Marspot.dmg>}"
HOST="${HOST:-mini}"
GUI_UID="${GUI_UID:-501}"
WAIT="${FRESH_WAIT:-420}"
RUN_ID="fresh-$$-$(date +%s)"
REMOTE_DMG="/tmp/marspot-$RUN_ID.dmg"

scp -q "$DMG" "$HOST:$REMOTE_DMG"

ssh "$HOST" "sudo -n launchctl asuser $GUI_UID sudo -u \$(id -un $GUI_UID) /usr/local/bin/bench-lock heavy --max-wait 1800 /bin/bash -s -- $REMOTE_DMG $RUN_ID $WAIT" <<'REMOTE'
set -uo pipefail
DMG="$1"; RUN_ID="$2"; WAIT="$3"
FRESH="$(mktemp -d "/tmp/marspot-$RUN_ID.XXXXXX")"
MNT="$FRESH/mnt"
STATE="$FRESH/Library/Application Support/marspot"
LOG="$FRESH/Library/Logs/Marspot/marspot.log"
SETTINGS="$FRESH/.claude/settings.json"
fail=0
check() { if "${@:2}"; then echo "  ok    $1"; else echo "  FAIL  $1"; fail=1; fi; }
ours() { ps -axww -E -o pid=,command= | grep -F "HOME=$FRESH" | grep -v grep; }

cleanup() {
  for sig in TERM KILL; do
    for p in $(ours | awk '{print $1}'); do kill -$sig "$p" 2>/dev/null; done
    sleep 2
  done
  hdiutil detach "$MNT" -quiet 2>/dev/null
  rm -rf "$FRESH" "$DMG"
}
trap cleanup EXIT

# read whole, then matched: `grep -q` hanging up early fails the pipe
notarised() { local out; out="$(spctl --assess -v "$@" 2>&1)"; [[ "$out" == *"Notarized Developer ID"* ]]; }
echo "==> the image, as Gatekeeper sees it"
check "disk image is notarised" notarised --type open --context context:primary-signature "$DMG"
mkdir -p "$MNT" "$FRESH/Applications" "$FRESH/tmp"
hdiutil attach "$DMG" -nobrowse -readonly -mountpoint "$MNT" -quiet || { echo "cannot mount"; exit 1; }
cp -R "$MNT/Marspot.app" "$FRESH/Applications/"
hdiutil detach "$MNT" -quiet
APP="$FRESH/Applications/Marspot.app"
check "app is notarised" notarised --type execute "$APP"
check "bundle carries marspot-peer" test -x "$APP/Contents/MacOS/marspot-peer"

echo "==> first launch, empty HOME"
env -i HOME="$FRESH" USER="$(id -un)" LOGNAME="$(id -un)" SHELL=/bin/zsh \
  PATH=/usr/bin:/bin:/usr/sbin:/sbin TMPDIR="$FRESH/tmp" LANG=en_US.UTF-8 \
  "$APP/Contents/MacOS/marspot-shell" >"$FRESH/stdout.log" 2>&1 &
SHELL_PID=$!

codex_bin() { [ -x "$FRESH/.local/bin/codex" ] || ls "$FRESH"/.codex/packages/standalone/current/bin/codex >/dev/null 2>&1; }
clis() { [ -x "$FRESH/.local/bin/claude" ] && codex_bin; }
hooks() { grep -q 'cc-statusline' "$SETTINGS" 2>/dev/null && grep -q 'claude-hook' "$SETTINGS" 2>/dev/null; }
deadline=$(( $(date +%s) + WAIT ))
until { clis && hooks; } || [ "$(date +%s)" -ge "$deadline" ] || ! kill -0 $SHELL_PID 2>/dev/null; do sleep 5; done

runs() { env -i HOME="$FRESH" PATH="$FRESH/.local/bin:/usr/bin:/bin" "$1" --version >/dev/null 2>&1; }
# the shell a pane runs: a child of a session process of ours
pane_shells() { for s in $(ours | awk '$2 ~ /marspot-session$/ {print $1}'); do pgrep -P "$s"; done; }
pane_path() {
  local p env
  for p in $(pane_shells); do
    env="$(ps -ww -E -o command= -p "$p")"
    [[ "$env" == *"PATH=$FRESH/.local/bin:"* ]] && return 0
  done
  return 1
}
# a gemini the machine already had is left alone, and that counts
gemini_there() { runs gemini || /bin/zsh -ilc 'command -v gemini' >/dev/null 2>&1; }
no_panic() { ! grep -qi 'panic' "$LOG" "$FRESH/stdout.log"; }
no_errors() { ! awk -F'\t' '$3=="ERROR"' "$LOG" | grep -q .; }
one_pane() { [ "$(ls "$STATE/sessions" 2>/dev/null | wc -l)" -ge 1 ]; }

echo "==> what is running"
ours | awk '{n=split($2,a,"/"); print "  " $1 "  " a[n]}'
for p in $(pane_shells); do echo "  $p  $(ps -o comm= -p "$p") (pane shell)"; done
echo "==> what it did"
check "shell is still running" kill -0 $SHELL_PID
check "core booted" grep -q CORE_BOOT "$LOG"
check "state is under Application Support" test -d "$STATE/sessions"
check "nothing under Caches" test ! -e "$FRESH/Library/Caches/marspot"
check "a pane has a shell" one_pane
check "the pane's PATH has ~/.local/bin" pane_path
for cli in claude codex; do check "$cli installed and runs" runs "$cli"; done
check "gemini installed, or already on this machine" gemini_there
check "status-line hook in the new claude config" grep -q 'cc-statusline' "$SETTINGS"
check "pane-name hooks in the new claude config" grep -q 'claude-hook' "$SETTINGS"
check "claude settings still parse" plutil -convert xml1 -o /dev/null "$SETTINGS"
check "profiles skill in the claude config" test -x "$FRESH/.claude/skills/marspot-profiles/profiles.sh"
check "codex has the peer server" grep -q 'mcp_servers.marspot' "$FRESH/.codex/config.toml"
check "no panic" no_panic
check "no ERROR lines" no_errors

echo "==> warnings in the log"
awk -F'\t' '$3=="WARN"{print "  " $7 "  " substr($8,1,140)}' "$LOG" | sort | uniq -c | sort -rn | head -20
echo "==> installs"
awk -F'\t' '$7 ~ /agents\./{print "  " $1 "  " $7 "  " $8 "  " $9 "  " $10}' "$LOG"
echo "==> stdout/stderr of the app (tail)"
tail -5 "$FRESH/stdout.log" | sed 's/^/  /'
exit $fail
REMOTE
