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
# the config dirs the installers create.  And what it must not do: put
# anything on screen but its main window, ask for more than one
# system permission at its first run, or run an agent CLI beyond the
# installs' own version checks: this host is a runner, and a stand-in
# codex usage feed keeps the app's usage collector off `codex app-server`.
#
# It reaches the network (three vendor downloads) and takes a few
# minutes.  Everything it starts carries the scratch HOME in its
# environment or runs from a binary under it, and that is how the
# cleanup finds it: nothing else on the host is touched.
#
# Exit 0 every check passed, 1 one failed, 3 none failed but the prompt
# count could not be measured on this host.
set -euo pipefail

DMG="${1:?usage: $0 <Marspot.dmg>}"
HOST="${HOST:-mini}"
WAIT="${FRESH_WAIT:-420}"
RUN_ID="fresh-$$-$(date +%s)"
REMOTE_DMG="/tmp/marspot-$RUN_ID.dmg"
# shellcheck source=bin/_lock-mark.sh
source "$(dirname "$0")/_lock-mark.sh"

scp -q "$DMG" "$HOST:$REMOTE_DMG"
scp -q "$(dirname "$0")/_tcc-prompts.sh" "$HOST:/tmp/marspot-$RUN_ID-tcc.sh"

ssh "$HOST" "/usr/local/bin/bench-lock heavy --max-wait 1800 $(lock_until $((WAIT + 600)) fresh-install "$RUN_ID") /bin/bash -s -- $REMOTE_DMG $RUN_ID $WAIT ${FRESH_ENV:-}" <<'REMOTE'
set -uo pipefail
DMG="$1"; RUN_ID="$2"; WAIT="$3"
# one more NAME=value for the app
EXTRA="${4:-MARSPOT_FRESH=1}"
FRESH="$(mktemp -d "/tmp/marspot-$RUN_ID.XXXXXX")" || exit 1
# launchd and bundle APIs report the resolved /private/tmp spelling
REAL="$(cd "$FRESH" && pwd -P)" || exit 1
MNT="$FRESH/mnt"
STATE="$FRESH/Library/Application Support/marspot"
LOG="$FRESH/Library/Logs/Marspot/marspot.log"
SETTINGS="$FRESH/.claude/settings.json"
TCC_PROMPTS="/tmp/marspot-$RUN_ID-tcc.sh"
fail=0
unmeasured=0
check() { if "${@:2}"; then echo "  ok    $1"; else echo "  FAIL  $1"; fail=1; fi; }
ours() {
  ps -axww -E -o pid=,command= | awk -v home="HOME=$FRESH" -v fresh="$FRESH/" -v real="$REAL/" \
    'index($0, home) || index($2, fresh) == 1 || index($2, real) == 1'
}

cleanup() {
  [ -n "${SAMPLER_PID:-}" ] && kill "$SAMPLER_PID" 2>/dev/null
  for sig in TERM KILL; do
    for p in $(ours | awk '{print $1}'); do kill -$sig "$p" 2>/dev/null; done
    sleep 2
  done
  hdiutil detach "$MNT" -quiet 2>/dev/null
  rm -rf "${FRESH:?}" "${DMG:?}" "${TCC_PROMPTS:?}"
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
# The pane's shell, with a note of the environment it was started in:
# `ps` cannot read a login zsh's, so the shell writes it down itself.
# Only the pane starts it with no arguments.
cat > "$FRESH/pane-shell" <<'SH'
#!/bin/sh
[ $# -eq 0 ] && env > "$HOME/pane.env"
exec /bin/zsh "$@"
SH
chmod +x "$FRESH/pane-shell"
# The system's own dialogs on screen, by window number, so the ones
# already up are not counted as this run's.  Seen on mini (macOS 27):
# with older dialogs left unanswered, a new permission prompt shows no
# window at all, so these are reported and the log decides.
prompt_windows() {
  /usr/bin/osascript -l JavaScript -e 'ObjC.import("CoreGraphics");
  var all = ObjC.deepUnwrap(ObjC.castRefToObject($.CGWindowListCopyWindowInfo($.kCGWindowListOptionOnScreenOnly, 0)));
  var owners = ["UserNotificationCenter", "universalAccessAuthWarn", "CoreServicesUIAgent", "tccd"];
  all.filter(function (w) { return owners.indexOf(w.kCGWindowOwnerName) >= 0; })
    .map(function (w) { return w.kCGWindowNumber + " " + w.kCGWindowOwnerName; }).join("\n")' | sort
}
prompt_windows > "$FRESH/prompt-windows.before"
# Real agent CLIs never run on a runner.  The app's usage collector asks
# each installed codex through `codex app-server` (first pass 10 s after
# launch, then every 300 s), and it stands down for any provider whose
# external feed is fresh.  So a stand-in external codex feed, fresh for
# the whole run, keeps the collector off codex; the claude and kimi
# passes start no agent CLI and stay as a fresh Mac has them.
mkdir -p "$FRESH/.local/state/devops"
printf '{"generated_at":"%s","accounts":[]}\n' "$(date -u -v+3H +%Y-%m-%dT%H:%M:%SZ)" \
  > "$FRESH/.local/state/devops/codex-usage.json"
# Every agent CLI process under the app, sampled each half second while
# it runs, as "<pid> <argv>".  Under the app means descended from the
# shell, from a pane's shell (launchd's child), carrying HOME=$FRESH or
# run from a binary under $FRESH (a LaunchAgent would be launchd's child):
# `ps` shows no environment for Apple's own binaries, a zsh among them.
# Allowed: `--version` (the app checks each install with it) and the
# claude bootstrap's `install`; anything else, `codex app-server` above
# all, fails the run.  FRESH_CHECK marks the CLIs this script runs itself.
# by the registry: launchd starts a pane's shell, so it is not its session's child
pane_shells() { sed -n 's/^shell_child_pid = \([0-9][0-9]*\)$/\1/p' "$STATE"/sessions/*/entry.toml 2>/dev/null; }
agent_procs() {
  local roots
  roots=" $(cat "$FRESH/roots" 2>/dev/null) $(pane_shells | tr '\n' ' ') "
  ps -axww -E -o pid=,ppid=,command= | awk -v home="HOME=$FRESH" -v fresh="$FRESH/" -v real="$REAL/" -v roots="$roots" '
    function env(s) { return s ~ /^[A-Za-z_][A-Za-z0-9_]*=/ }
    { pid[NR] = $1; ppid[NR] = $2; row[NR] = $0
      mine = index($0, home) || index($3, fresh) == 1 || index($3, real) == 1
      if (index(roots, " " $1 " ") || (mine && !index($0, " FRESH_CHECK=1"))) under[$1] = 1 }
    END {
      for (grew = 1; grew; ) { grew = 0
        for (r = 1; r <= NR; r++) if (!(pid[r] in under) && (ppid[r] in under)) { under[pid[r]] = 1; grew = 1 } }
      for (r = 1; r <= NR; r++) { if (!(pid[r] in under)) continue
        nf = split(row[r], f, " ")
        for (k = 3; k <= 4 && k <= nf; k++) { n = split(f[k], a, "/"); if (a[n] ~ /^(claude|codex|gemini|kimi)([-.]|$)/) break }
        if (k > 4 || k > nf) continue
        line = f[1]; for (i = 3; i <= nf && !(i > k && env(f[i])); i++) line = line " " f[i]
        nxt = (k + 1 <= nf && !env(f[k + 1])) ? f[k + 1] : ""
        if (nxt == "--version" && (k + 2 > nf || env(f[k + 2]))) continue
        if (a[n] ~ /^claude/ && nxt == "install") continue
        print line } }'
}
sample_agents() {
  while :; do
    echo tick
    agent_procs
    sleep 0.5
  done > "$FRESH/agents.seen" 2>&1
}
# The instrument has to be seen to see: a process named codex under the
# same HOME must show up in the samples.  An ad hoc signed copy of sleep,
# because `ps` hides an Apple binary's environment.
mkdir -p "$FRESH/probe"
cp /bin/sleep "$FRESH/probe/codex"
codesign -f -s - "$FRESH/probe/codex" 2>/dev/null
sample_agents &
SAMPLER_PID=$!
# no "Terminated" notice when it is stopped below
disown "$SAMPLER_PID"
env -i HOME="$FRESH" "$FRESH/probe/codex" 2 &
PROBE_PID=$!
wait $PROBE_PID
STARTED="$(date '+%Y-%m-%d %H:%M:%S')"
env -i HOME="$FRESH" USER="$(id -un)" LOGNAME="$(id -un)" SHELL="$FRESH/pane-shell" \
  PATH=/usr/bin:/bin:/usr/sbin:/sbin TMPDIR="$FRESH/tmp" LANG=en_US.UTF-8 "$EXTRA" \
  "$APP/Contents/MacOS/marspot-shell" >"$FRESH/stdout.log" 2>&1 &
SHELL_PID=$!
echo "$SHELL_PID" > "$FRESH/roots"

codex_bin() { [ -x "$FRESH/.local/bin/codex" ] || ls "$FRESH"/.codex/packages/standalone/current/bin/codex >/dev/null 2>&1; }
clis() { [ -x "$FRESH/.local/bin/claude" ] && codex_bin && [ -x "$FRESH/.kimi-code/bin/kimi" ]; }
kimi_peer() { grep -q '"marspot"' "$FRESH/.kimi-code/mcp.json" 2>/dev/null && grep -q 'kimi-hook' "$FRESH/.kimi-code/config.toml" 2>/dev/null; }
hooks() { grep -q 'cc-statusline' "$SETTINGS" 2>/dev/null && grep -q 'claude-hook' "$SETTINGS" 2>/dev/null && kimi_peer; }
deadline=$(( $(date +%s) + WAIT ))
until { clis && hooks; } || [ "$(date +%s)" -ge "$deadline" ] || ! kill -0 $SHELL_PID 2>/dev/null; do sleep 5; done

runs() { env -i HOME="$FRESH" FRESH_CHECK=1 PATH="$FRESH/.local/bin:/usr/bin:/bin" "$1" --version >/dev/null 2>&1; }
pane_path() { grep -q "^PATH=$FRESH/.local/bin:" "$FRESH/pane.env"; }
pane_path_kimi() { grep -q "^PATH=.*:$FRESH/.kimi-code/bin:" "$FRESH/pane.env"; }
kimi_runs() { env -i HOME="$FRESH" FRESH_CHECK=1 PATH=/usr/bin:/bin "$FRESH/.kimi-code/bin/kimi" --version >/dev/null 2>&1; }
kimi_doctor() { env -i HOME="$FRESH" FRESH_CHECK=1 PATH=/usr/bin:/bin "$FRESH/.kimi-code/bin/kimi" doctor >/dev/null 2>&1; }
# a gemini the machine already had is left alone, and that counts
gemini_there() { runs gemini || /bin/zsh -ilc 'command -v gemini' >/dev/null 2>&1; }
no_panic() { ! grep -qi 'panic' "$LOG" "$FRESH/stdout.log"; }
no_errors() { ! awk -F'\t' '$3=="ERROR"' "$LOG" | grep -q .; }
one_pane() { [ "$(ls "$STATE/sessions" 2>/dev/null | wc -l)" -ge 1 ]; }
# The windows the app has on screen, one per line as
# "<layer> <width>x<height>", as the window server lists them: an
# ordinary window is on layer 0.  This much needs no permission; a
# title would need Screen Recording.
app_windows() {
  /usr/bin/osascript -l JavaScript -e 'function run(argv) {
  ObjC.import("CoreGraphics");
  var all = ObjC.deepUnwrap(ObjC.castRefToObject($.CGWindowListCopyWindowInfo($.kCGWindowListOptionOnScreenOnly, 0)));
  return all.filter(function (w) { return String(w.kCGWindowOwnerPID) === argv[0]; })
    .map(function (w) { return w.kCGWindowLayer + " " + w.kCGWindowBounds.Width + "x" + w.kCGWindowBounds.Height; })
    .join("\n");
}' "$SHELL_PID"
}
one_main_window() { local w; w="$(app_windows)"; [ "$(grep -c . <<<"$w")" -eq 1 ] && [[ $w == "0 "* ]]; }

echo "==> what is running"
ours | awk '{n=split($2,a,"/"); print "  " $1 "  " a[n]}'
for p in $(pane_shells); do echo "  $p  $(ps -o comm= -p "$p") (pane shell)"; done
echo "==> what is on screen"
app_windows | sed 's/^/  /'
echo "==> system permission requests since $STARTED"
# the log names the app's copy by its real path, /private/tmp/...
"$TCC_PROMPTS" "$STARTED" "$(cd "$FRESH" && pwd -P)/" > "$FRESH/prompts" 2>&1
tcc_rc=$?
sed '/^requests=/d' "$FRESH/prompts" | sed 's/^/  /'
tally="$(grep '^requests=' "$FRESH/prompts" || true)"
prompts="$(sed -n 's/.*prompts=\([0-9]*\).*/\1/p' <<<"$tally")"
stored="$(sed -n 's/.*stored=\([0-9]*\).*/\1/p' <<<"$tally")"
prompt_windows | comm -13 "$FRESH/prompt-windows.before" - > "$FRESH/prompt-windows.new"
echo "  ${tally:-no tally} (unified log, tccd AUTHREQ_*; exit $tcc_rc);" \
  "$(grep -c . "$FRESH/prompt-windows.new") new system dialogs on screen"
sed 's/^/  new dialog: /' "$FRESH/prompt-windows.new"
echo "==> what it did"
check "shell is still running" kill -0 $SHELL_PID
check "one window on screen, the main one" one_main_window
check "core booted" grep -q CORE_BOOT "$LOG"
check "state is under Application Support" test -d "$STATE/sessions"
check "nothing under Caches" test ! -e "$FRESH/Library/Caches/marspot"
check "a pane has a shell" one_pane
check "the pane's PATH has ~/.local/bin" pane_path
check "the pane's PATH has ~/.kimi-code/bin" pane_path_kimi
for cli in claude codex; do check "$cli installed and runs" runs "$cli"; done
check "gemini installed, or already on this machine" gemini_there
check "kimi installed and runs" kimi_runs
check "kimi has the peer server and the inbox hooks" kimi_peer
check "kimi accepts the configuration it was given" kimi_doctor
check "status-line hook in the new claude config" grep -q 'cc-statusline' "$SETTINGS"
check "pane-name hooks in the new claude config" grep -q 'claude-hook' "$SETTINGS"
check "claude settings still parse" plutil -convert xml1 -o /dev/null "$SETTINGS"
check "profiles skill in the claude config" test -x "$FRESH/.claude/skills/marspot-profiles/profiles.sh"
check "codex has the peer server" grep -q 'mcp_servers.marspot' "$FRESH/.codex/config.toml"
check "no panic" no_panic
check "no ERROR lines" no_errors
kill "$SAMPLER_PID" 2>/dev/null
SAMPLER_PID=""
samples="$(grep -c '^tick$' "$FRESH/agents.seen")"
probe_seen() { grep -q "^$PROBE_PID .*/probe/codex 2$" "$FRESH/agents.seen"; }
grep -v -e '^tick$' -e "^$PROBE_PID " "$FRESH/agents.seen" | sort -u > "$FRESH/agents.bad"
no_agents() { [ "$samples" -gt 0 ] && [ ! -s "$FRESH/agents.bad" ]; }
collector_stood_down() {
  awk -F'\t' '$7=="usage.collector" && /provider=codex/ && /by=devops/' "$LOG" | grep -q . \
    && test ! -e "$STATE/usage/codex-usage.json"
}
check "the sampler saw its own process named codex" probe_seen
check "no agent CLI process under the app but --version and the claude installer ($samples samples)" no_agents
sed 's/^/  agent process: /' "$FRESH/agents.bad"
check "the usage collector left codex to the stand-in feed" collector_stood_down
# A request the login user's TCC database already answers, allowed or
# denied, is not asked again, and that database is the user's, not
# HOME's: the empty HOME does not make it new.  So the count is a
# verdict only when the log is readable and tccd answered none of the
# app's requests from a stored decision; otherwise it is not measured
# (exit 3), which is neither a pass nor a failure.  More than one prompt
# fails either way.
# redaction can only hide requests, so a count over one fails even then
if [[ $prompts =~ ^[0-9]+$ ]] && (( prompts > 1 )); then
  echo "  FAIL  at most one system permission prompt ($prompts asked)"; fail=1
elif [[ $tcc_rc != 0 || ! $prompts =~ ^[0-9]+$ ]]; then
  echo "  NOT MEASURED  at most one system permission prompt" \
    "(the log cannot be read, or tccd logged nothing readable: the instrument cannot see)"
  unmeasured=1
elif (( stored > 0 )); then
  echo "  NOT MEASURED  at most one system permission prompt ($prompts asked;" \
    "$stored answered from $(id -un)'s stored decisions, which a Mac that never ran marspot does not have)"
  unmeasured=1
else
  echo "  ok    at most one system permission prompt ($prompts asked; tccd evaluated every request afresh)"
fi

echo "==> warnings in the log"
awk -F'\t' '$3=="WARN"{print "  " $7 "  " substr($8,1,140)}' "$LOG" | sort | uniq -c | sort -rn | head -20
echo "==> what the plugins that write the configuration logged"
awk -F'\t' '$7 ~ /peerbus|claudecode|skills|profiles|hook/{print "  " $1 "  " $7 "  " substr($8,1,110)}' "$LOG" | tail -12
echo "==> installs"
awk -F'\t' '$7 ~ /agents\./{print "  " $1 "  " $7 "  " $8 "  " $9 "  " $10}' "$LOG"
echo "==> stdout/stderr of the app (tail)"
tail -5 "$FRESH/stdout.log" | sed 's/^/  /'
exit $(( fail ? 1 : unmeasured ? 3 : 0 ))
REMOTE
