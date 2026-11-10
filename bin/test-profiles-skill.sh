#!/usr/bin/env bash
# bin/test-profiles-skill.sh — exercise assets/skills/marspot-profiles/profiles.sh
# against a scratch HOME.
#
# Pure bash on a temp dir: no build, no window, nothing outside the scratch
# tree.  Every invocation of the script gets HOME pointed at the scratch dir,
# and MARSPOT_PROFILES_SKIP_RUNNING_CHECK=1 because pgrep would otherwise see
# the agents that are really running on this machine.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT="$ROOT/assets/skills/marspot-profiles/profiles.sh"
REAL_HOME="$HOME"
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT
H="$SCRATCH/home"
mkdir -p "$H"

CHECKS=0
fail() {
  echo "FAIL: $*" >&2
  exit 1
}
ok() {
  CHECKS=$((CHECKS + 1))
}

# the script under test, always with the scratch HOME
p() {
  HOME="$H" MARSPOT_PROFILES_SKIP_RUNNING_CHECK=1 /bin/bash "$SCRIPT" "$@"
}

[ "$H" != "$REAL_HOME" ] || fail "scratch HOME is the real HOME"
case "$H" in
  "$SCRATCH"/*) ok ;;
  *) fail "scratch HOME $H is not under $SCRATCH" ;;
esac
[ "$(HOME="$H" /bin/bash -c 'echo "$HOME"')" = "$H" ] || fail "HOME override does not reach the child"
REAL_BEFORE="$(ls -la "$REAL_HOME" | grep -E ' \.(claude|codex|gemini)' || true)"

is_link_to() {
  [ -L "$1" ] || fail "$1 is not a symlink"
  [ "$(readlink "$1")" = "$2" ] || fail "$1 -> $(readlink "$1"), expected $2"
  [ -e "$1" ] || fail "$1 is dangling"
  ok
}
is_real_file() {
  [ -f "$1" ] && [ ! -L "$1" ] || fail "$1 is not a real file"
  ok
}
is_real_dir() {
  [ -d "$1" ] && [ ! -L "$1" ] || fail "$1 is not a real directory"
  ok
}
absent() {
  [ ! -e "$1" ] && [ ! -L "$1" ] || fail "$1 exists"
  ok
}
has() {
  [ "$(cat "$1")" = "$2" ] || fail "$1 holds '$(cat "$1")', expected '$2'"
  ok
}
snapshot() {
  (cd "$H" && find . -exec ls -ld {} + | awk '{ $6 = $7 = $8 = ""; print }' | sort)
}

# default directories as a fresh user has them, one file per kind of entry
mkdir -p "$H/.claude/projects/-w-app" "$H/.claude/sessions" "$H/.claude/skills/mine" \
  "$H/.claude/plugins/cache/pk" "$H/.claude/commands" "$H/.claude/statsig"
echo memory >"$H/.claude/CLAUDE.md"
echo '{"model":"opus"}' >"$H/.claude/settings.json"
echo hist >"$H/.claude/history.jsonl"
echo sess >"$H/.claude/projects/-w-app/a.jsonl"
echo skill >"$H/.claude/skills/mine/SKILL.md"
echo cached >"$H/.claude/plugins/cache/pk/f"
echo '[]' >"$H/.claude/plugins/blocklist.json"
echo installed >"$H/.claude/plugins/installed_plugins.json"
echo mine >"$H/.claude/commands/c.md"
echo stats >"$H/.claude/stats-cache.json"
echo dot >"$H/.claude/.last-cleanup"
echo identity >"$H/.claude.json"
# the shared dir already has one of the entries: it must stay behind
mkdir -p "$H/.claude-shared/commands"
echo theirs >"$H/.claude-shared/commands/c.md"

mkdir -p "$H/.codex/sessions/2026" "$H/.codex/skills" "$H/.codex/tmp"
echo agents >"$H/.codex/AGENTS.md"
echo 'model = "x"' >"$H/.codex/config.toml"
echo token >"$H/.codex/auth.json"
echo hist >"$H/.codex/history.jsonl"
echo id >"$H/.codex/installation_id"
echo db >"$H/.codex/state_5.sqlite"
echo roll >"$H/.codex/sessions/2026/r.jsonl"

mkdir -p "$H/.gemini/tmp/app/chats" "$H/.gemini/skills" "$H/.gemini/history/app"
echo '{}' >"$H/.gemini/settings.json"
echo ctx >"$H/.gemini/GEMINI.md"
echo token >"$H/.gemini/oauth_creds.json"
echo acct >"$H/.gemini/google_accounts.json"
echo reg >"$H/.gemini/projects.json"
echo id >"$H/.gemini/installation_id"
echo chat >"$H/.gemini/tmp/app/chats/session-1.jsonl"

echo '# my rc' >"$H/.zshrc"
echo '# my bashrc' >"$H/.bashrc"

# a dry run prints the moves and makes none
before="$(snapshot)"
for a in claude codex gemini; do
  out="$(p init "$a" --dry-run)"
  echo "$out" | grep -q "^would run: mv " || fail "init $a --dry-run printed no moves"
  ok
done
[ "$(snapshot)" = "$before" ] || fail "a dry run changed the tree"
ok

# a running agent stops init before anything moves
mkdir -p "$SCRATCH/fakebin"
printf '#!/bin/sh\necho 4242\n' >"$SCRATCH/fakebin/pgrep"
chmod +x "$SCRATCH/fakebin/pgrep"
if out="$(HOME="$H" PATH="$SCRATCH/fakebin:$PATH" /bin/bash "$SCRIPT" init codex 2>&1)"; then
  fail "init ran while codex was running"
fi
echo "$out" | grep -q "codex is running (pid 4242" || fail "no running-agent message: $out"
[ "$(snapshot)" = "$before" ] || fail "a refused init changed the tree"
ok

for a in claude codex gemini; do
  p init "$a"
done

# claude
is_link_to "$H/.claude" "$H/.claude-profile-1"
for e in CLAUDE.md settings.json history.jsonl projects sessions skills commands plans plugins/cache plugins/blocklist.json; do
  is_link_to "$H/.claude-profile-1/$e" "$H/.claude-shared/$e"
done
has "$H/.claude-shared/projects/-w-app/a.jsonl" sess
has "$H/.claude-shared/history.jsonl" hist
has "$H/.claude-shared/plugins/cache/pk/f" cached
is_real_dir "$H/.claude-profile-1/plugins"
is_real_file "$H/.claude-profile-1/plugins/installed_plugins.json"
is_real_file "$H/.claude-profile-1/stats-cache.json"
is_real_file "$H/.claude-profile-1/.last-cleanup"
is_real_dir "$H/.claude-profile-1/statsig"
is_real_file "$H/.claude-profile-1/.claude.json"
has "$H/.claude-profile-1/.claude.json" identity
has "$H/.claude.json" identity
has "$H/.claude/CLAUDE.md" memory
# the entry the shared dir already had: shared copy kept, the other in the backup
has "$H/.claude-shared/commands/c.md" theirs
backup="$(echo "$H"/.claude.backup-*)"
is_real_dir "$backup"
has "$backup/commands/c.md" mine

# codex: the login moves with account 1
is_link_to "$H/.codex" "$H/.codex-profile-1"
for e in AGENTS.md config.toml sessions skills cache plugins; do
  is_link_to "$H/.codex-profile-1/$e" "$H/.codex-shared/$e"
done
for f in auth.json history.jsonl installation_id state_5.sqlite; do
  is_real_file "$H/.codex-profile-1/$f"
done
has "$H/.codex/auth.json" token
is_real_dir "$H/.codex-profile-1/tmp"
is_real_dir "$(echo "$H"/.codex.backup-*)"
absent "$H/.codex-profile-1/models_cache.json"

# gemini: the config dir is one level down
is_link_to "$H/.gemini" "$H/.gemini-profile-1/.gemini"
for e in settings.json GEMINI.md tmp skills history extensions; do
  is_link_to "$H/.gemini-profile-1/.gemini/$e" "$H/.gemini-shared/$e"
done
for f in oauth_creds.json google_accounts.json projects.json installation_id; do
  is_real_file "$H/.gemini-profile-1/.gemini/$f"
done
has "$H/.gemini/tmp/app/chats/session-1.jsonl" chat
is_real_dir "$(echo "$H"/.gemini.backup-*)"

# a second init does nothing
before="$(snapshot)"
for a in claude codex gemini; do
  out="$(p init "$a")"
  echo "$out" | grep -q "nothing to do" || fail "second init $a did not say it had nothing to do"
  ok
done
[ "$(snapshot)" = "$before" ] || fail "a second init changed the tree"
ok

# add: next number, a given number, refusal, dry run
p add claude >/dev/null
is_real_dir "$H/.claude-profile-2"
is_link_to "$H/.claude-profile-2/projects" "$H/.claude-shared/projects"
is_link_to "$H/.claude-profile-2/history.jsonl" "$H/.claude-shared/history.jsonl"
is_link_to "$H/.claude-profile-2/plugins/cache" "$H/.claude-shared/plugins/cache"
absent "$H/.claude-profile-2/.claude.json"
out="$(p add codex 5)"
echo "$out" | grep -q "codex5 login" || fail "add codex 5 did not say how to log in"
is_link_to "$H/.codex-profile-5/config.toml" "$H/.codex-shared/config.toml"
absent "$H/.codex-profile-5/auth.json"
p add gemini >/dev/null
is_link_to "$H/.gemini-profile-2/.gemini/tmp" "$H/.gemini-shared/tmp"
absent "$H/.gemini-profile-2/.gemini/oauth_creds.json"
if p add claude 2 2>/dev/null; then
  fail "add claude 2 succeeded although the profile exists"
fi
ok
out="$(p add claude --dry-run)"
echo "$out" | grep -q "would run: mkdir -p $H/.claude-profile-3" || fail "add --dry-run printed no mkdir"
absent "$H/.claude-profile-3"
if p add claude 0 2>/dev/null || p add nosuch 2>/dev/null; then
  fail "a bad number or agent was accepted"
fi
ok

# aliases: one block, one backup, stable on the second run, and it works
p aliases >/dev/null
for rc in .zshrc .bashrc; do
  [ "$(grep -c '^# >>> marspot-profiles' "$H/$rc")" = 1 ] || fail "$rc does not have exactly one block"
  is_real_file "$H/$rc.marspot-profiles.bak"
done
has "$H/.zshrc.marspot-profiles.bak" '# my rc'
head -1 "$H/.zshrc" | grep -qx '# my rc' || fail "the rc lost its own content"
sum="$(cat "$H/.zshrc" "$H/.bashrc" | cksum)"
p aliases >/dev/null
[ "$(cat "$H/.zshrc" "$H/.bashrc" | cksum)" = "$sum" ] || fail "a second aliases run changed the rc files"
ok
has "$H/.zshrc.marspot-profiles.bak" '# my rc'
out="$(HOME="$H" zsh -f -c 'source "$HOME/.zshrc" && alias claude2 codex5 gemini2')"
echo "$out" | grep -q "claude2='CLAUDE_CONFIG_DIR='\\\\''$H/.claude-profile-2'\\\\'' claude'" || fail "zsh alias claude2 is wrong: $out"
echo "$out" | grep -q "GEMINI_CLI_HOME=.*$H/.gemini-profile-2" || fail "zsh alias gemini2 is wrong: $out"
ok
out="$(HOME="$H" /bin/bash -c 'shopt -s expand_aliases; source "$HOME/.bashrc"; alias codex5; shopt nullglob || true')"
echo "$out" | grep -q "CODEX_HOME=.*$H/.codex-profile-5" || fail "bash alias codex5 is wrong: $out"
echo "$out" | grep -q "nullglob.*off" || fail "the bash block left nullglob on: $out"
ok
# a home with no profiles: the block must not error
mkdir -p "$SCRATCH/empty"
HOME="$SCRATCH/empty" /bin/bash "$SCRIPT" aliases >/dev/null
HOME="$SCRATCH/empty" zsh -f -c 'source "$HOME/.zshrc"' || fail "the zsh block errors when there are no profiles"
ok

# doctor: clean, then each kind of damage
p doctor >/dev/null || fail "doctor reports problems on a fresh layout: $(p doctor || true)"
out="$(p doctor)"
echo "$out" | tail -1 | grep -qx "all clear" || fail "doctor did not end in all clear"
ok

expect_problem() {
  local out
  if out="$(p doctor)"; then
    fail "doctor exited 0 with $1 broken"
  fi
  echo "$out" | grep -q "^problem: .*$1" || fail "doctor did not name $1: $out"
  ok
}
rm "$H/.claude-profile-2/projects"
expect_problem "$H/.claude-profile-2/projects is missing"
ln -s "$H/.claude-shared/nowhere" "$H/.claude-profile-2/projects"
expect_problem "$H/.claude-profile-2/projects points to"
rm "$H/.claude-profile-2/projects"
mkdir "$H/.claude-profile-2/projects"
expect_problem "$H/.claude-profile-2/projects is not a symlink"
rmdir "$H/.claude-profile-2/projects"
ln -s "$H/.claude-shared/projects" "$H/.claude-profile-2/projects"
p doctor >/dev/null || fail "doctor still reports problems after the repair"
ok

ln -s "$H/.codex-profile-1/auth.json" "$H/.codex-profile-5/auth.json"
expect_problem "$H/.codex-profile-5/auth.json is a symlink"
rm "$H/.codex-profile-5/auth.json"

mv "$H/.gemini-shared/tmp" "$H/.gemini-shared/tmp.gone"
expect_problem "$H/.gemini-profile-2/.gemini/tmp is a dangling symlink"
mv "$H/.gemini-shared/tmp.gone" "$H/.gemini-shared/tmp"

sed -i '' '/marspot-profiles/d' "$H/.zshrc"
expect_problem "no alias block in $H/.zshrc"
p aliases >/dev/null
p doctor >/dev/null || fail "doctor still reports problems after the last repair"
ok

[ "$(ls -la "$REAL_HOME" | grep -E ' \.(claude|codex|gemini)' || true)" = "$REAL_BEFORE" ] \
  || fail "the real HOME changed during the test"
ok

echo "test-profiles-skill: PASS ($CHECKS checks)"
