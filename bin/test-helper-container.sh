#!/usr/bin/env bash
#
# A container helper's script against a real docker, in one real session
# process.
#
# It opens no window and starts no app, so it may run on the machine
# someone is working on; it has to, as the runner has no container
# runtime.  It starts one session and one small container at a time,
# with a stand-in agent: no account, no token and no model are used.
#
# For claude (token by env file), gemini and kimi (credentials mounted):
#   - the working directory, the result directory, the helper's own home
#     and the credentials are mounted where the agent looks for them, and
#     nothing else is, what the account's links lead to among it;
#   - credentials renewed inside as the agent does it are renewed outside;
#   - git's config and hooks cannot be written to;
#   - the agent writes /marspot/result.md, which is in the helper's
#     directory on the host, and reads what is typed into the pane;
#   - the container is gone within 2 s of the session's end, by a hangup
#     and by a kill, while the agent prints nothing.
#
# Every docker command has a time limit.  What it makes (the containers,
# its image, the base image if it was not there) is removed, and so are
# the processes it started.  Other containers and images may come and go
# meanwhile: only names of this test are looked at.
# Exits 0 when every check passed, 2 when there is no running docker.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 1
DOCKER="$(command -v docker || true)"
TAG="marspot-helper-test:$$"
BASE=busybox:1.36
SB="$(mktemp -d "$HOME/.marspot-helper-container.XXXXXX")" || exit 1
failures=0 checks=0

# seconds, command…: at the limit, or at this script's end, the command's
# whole process group is ended
limit() {
  perl -e 'my $t = shift; my $p = fork; die unless defined $p;
    if (!$p) { setpgrp; exec @ARGV; exit 127 }
    for my $s (qw(ALRM TERM INT HUP)) { $SIG{$s} = sub { kill "KILL", -$p; exit 124 } }
    alarm $t; waitpid $p, 0; exit($? & 127 ? 128 + ($? & 127) : $? >> 8)' "$@"
}
dk() { limit 30 "$DOCKER" "$@"; }
has_image() { dk image inspect "$1" --format '{{.Id}}'; }
none_of() { # sid
  local out
  out=$(dk ps -aq --filter "label=marspot.helper=$1") && [[ -z $out ]]
}
no_image() {
  local out
  ! out=$(has_image "$1" 2>&1) && grep -qi "no such image" <<<"$out"
}
check() { # what, command…
  local what=$1
  shift
  checks=$((checks + 1))
  if "$@" >/dev/null 2>&1; then
    echo "  ok   $what"
  else
    echo "  FAIL $what"
    failures=$((failures + 1))
  fi
}
cleanup() {
  local n
  for n in "$SB"/*/name; do
    [[ -f $n ]] && dk rm -f "$(cat "$n")" >/dev/null 2>&1
  done
  # sessions of a test that was cut off, by the process they recorded
  for n in "$SB"/*/session.pid; do
    [[ -f $n ]] && kill -KILL "$(cat "$n")" 2>/dev/null
  done
  dk image rm -f "$TAG" >/dev/null 2>&1
  [[ ${base_was_there:-yes} == no ]] && dk image rm "$BASE" >/dev/null 2>&1
  rm -rf "${SB:?}"
}

if [[ -z $DOCKER ]] || ! dk version --format '{{.Server.Version}}' >/dev/null 2>&1; then
  rm -rf "${SB:?}"
  echo "no running docker on this machine" >&2
  exit 2
fi
trap cleanup EXIT
trap 'exit 130' INT TERM
base_was_there=yes
has_image "$BASE" >/dev/null 2>&1 || base_was_there=no

echo "════ build ════"
mkdir -p "$SB/image"
cat > "$SB/image/agent" <<'AGENT'
#!/bin/sh
# a stand-in agent: says what it sees in the result file, then is silent
{
  echo "agent ${0##*/}"
  echo "uid $(id -u)"
  echo "pwd $(pwd)"
  echo "work $(cat /work/given 2>&1)"
  for a in "$@"; do last=$a; done
  echo "task $(printf '%s' "$last" | head -1)"
  printf '%s\n' "$last" | grep -F "Say 'hi'" | sed 's/^/asked /'
  [ -n "$CLAUDE_CODE_OAUTH_TOKEN" ] && echo "token given"
  echo "args $(printf '%s ' "$@" | grep -c TOKEN)"
  [ -f "${CLAUDE_CONFIG_DIR:-/none}/.claude.json" ] && echo "config there"
  echo "git $(echo x 2>/dev/null >> /work/.git/config && echo written || echo kept) $(touch /work/.git/hooks/new 2>/dev/null && echo written || echo kept)"
  G=/home/node/.gemini K=/home/node/.kimi-code
  if [ -f $G/oauth_creds.json ]; then
    echo "login $(cat $G/oauth_creds.json) $(ls $G/linked $G/other 2>/dev/null | wc -l | tr -d ' ')"
    echo renewed > $G/t
    mv $G/t $G/oauth_creds.json 2>/dev/null && echo "replaced" || { cat $G/t > $G/oauth_creds.json; echo "in place"; }
  fi
  if [ -d $K/credentials ]; then
    echo "login $(cat $K/credentials/kimi-code.json) $(ls $K/linked $K/other 2>/dev/null | wc -l | tr -d ' ')"
    echo renewed > $K/credentials/t.json.tmp.1 && mv $K/credentials/t.json.tmp.1 $K/credentials/kimi-code.json && echo "replaced"
    mkdir $K/oauth/kimi-code.lock && rmdir $K/oauth/kimi-code.lock && echo "locked"
  fi
  echo "host $(ls -d /Users/*/.ssh /Users/*/Library 2>/dev/null | wc -l | tr -d ' ')"
  [ -t 0 ] && [ -t 2 ] && echo "terminal"
} > /marspot/result.md
echo "from inside" > /work/made
read -r typed
echo "typed $typed" >> /marspot/result.md
echo "END" >> /marspot/result.md
exec sleep 600
AGENT
cat > "$SB/image/Dockerfile" <<DOCKERFILE
FROM $BASE
RUN adduser -D -u 1000 node
COPY agent /usr/local/bin/claude
COPY agent /usr/local/bin/gemini
COPY agent /usr/local/bin/kimi
RUN chmod 755 /usr/local/bin/claude /usr/local/bin/gemini /usr/local/bin/kimi
USER node
DOCKERFILE
limit 180 "$DOCKER" build -q -t "$TAG" "$SB/image" >/dev/null || { echo "FAIL: the test image did not build" >&2; exit 1; }
limit 1500 cargo test -q --no-run --bin marspot-shell || exit 1
limit 1500 cargo test -q --no-run -p marspot-session --test it || exit 1

mkdir -p "$SB/work/.git/hooks" "$SB/account/.gemini/other" "$SB/account/credentials" "$SB/account/oauth" "$SB/account/other" "$SB/elsewhere" "$SB/state"
echo "from outside" > "$SB/work/given"
echo "[core]" > "$SB/work/.git/config"
echo "linked-file" > "$SB/elsewhere/file"
echo "x" > "$SB/account/other/file"
echo "x" > "$SB/account/.gemini/other/file"
ln -s "$SB/elsewhere" "$SB/account/linked"
ln -s "$SB/elsewhere" "$SB/account/.gemini/linked"
: > "$SB/account/oauth/kimi-code"
export CONTAINER_DOCKER="$DOCKER" CONTAINER_IMAGE="$TAG" CONTAINER_CWD="$SB/work" CONTAINER_HOME="$SB/account"

sid=$(( $$ % 100000 * 10 + 900000 ))
for run in claude:hangup claude:kill gemini:hangup kimi:kill; do
  agent=${run%%:*}
  sid=$((sid + 1))
  echo "════ $agent, ended by a ${run#*:} ════"
  export CONTAINER_AGENT=$agent CONTAINER_END=${run#*:} CONTAINER_DIR="$SB/$sid" CONTAINER_SID=$sid
  rm -f "$SB/work/made"
  echo "a-login" > "$SB/account/.gemini/oauth_creds.json"
  echo "a-login" > "$SB/account/credentials/kimi-code.json"
  check "the files are written" limit 120 env MARSPOT_STATE_DIR="$SB/state" cargo test -q --bin marspot-shell -- --ignored --exact plugins::helpers::container::tests::container_files_for_the_script
  # cargo's own success says nothing when no test matched
  check "and are there" test -s "$SB/$sid/run.sh" -a -s "$SB/$sid/name"
  limit 180 cargo test -q -p marspot-session --test it -- --ignored --nocapture --exact helper_container::helper_container_goes_with_its_session \
    > "$SB/$sid.log" 2>&1
  rc=$?
  grep -a '^gone ' "$SB/$sid.log" | sed 's/^/       /'
  check "the session ran it and took the container with it" test "$rc" = 0 -a -n "$(grep -a '^gone ' "$SB/$sid.log")"
  (( rc == 0 )) || tail -25 "$SB/$sid.log"
  R="$SB/$sid/share/result.md"
  for line in "agent $agent" "uid 1000" "pwd /work" "work from outside" "git kept kept" \
              "args 0" "host 0" "terminal" "typed hello"; do
    check "inside: $line" grep -qxF -- "$line" "$R"
  done
  # kimi's is typed in once its screen is up
  [[ $agent == kimi ]] || for line in "task Task from proj#12, delegated through marspot." "asked Say 'hi' to \$HOME \`now\`"; do
    check "inside: $line" grep -qxF -- "$line" "$R"
  done
  if [[ $agent == claude ]]; then
    check "inside: the token is in its environment" grep -qx "token given" "$R"
    check "inside: its config directory is there" grep -qx "config there" "$R"
    check "the token is on no command line" bash -c "! grep -q TOKEN-0123 '$SB/$sid/run.sh' && grep -q TOKEN-0123 '$SB/$sid/env'"
    check "the env file is private" test "$(stat -f %Lp "$SB/$sid/env")" = 600
  else
    check "inside: its login and nothing beside it" grep -qx "login a-login 0" "$R"
    creds="$SB/account/credentials/kimi-code.json" how="replaced"
    [[ $agent == gemini ]] && creds="$SB/account/.gemini/oauth_creds.json" how="in place"
    check "inside: renewed ($how)" grep -qx "$how" "$R"
    [[ $agent == kimi ]] && check "inside: it can take the lock" grep -qx locked "$R"
    check "and so it is outside" grep -qx renewed "$creds"
  fi
  check "what it made is in the working directory" grep -qx "from inside" "$SB/work/made"
  check "git's config is as it was" grep -qx '\[core\]' "$SB/work/.git/config"
  check "no container is left" none_of "$sid"
done

echo "════ clean up ════"
dk image rm -f "$TAG" >/dev/null 2>&1
[[ $base_was_there == no ]] && dk image rm "$BASE" >/dev/null 2>&1
for (( n = sid - 3; n <= sid; n++ )); do
  check "no container of this test is left ($n)" none_of "$n"
done
check "its image is gone" no_image "$TAG"
now_there=yes
has_image "$BASE" >/dev/null 2>&1 || now_there=no
check "the base image is as it was" test "$now_there" = "$base_was_there"

echo
echo "$((checks - failures))/$checks passed"
(( failures == 0 ))
