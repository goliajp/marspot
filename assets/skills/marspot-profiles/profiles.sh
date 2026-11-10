#!/bin/bash
# profiles.sh — several accounts of one coding agent on one machine.
#
#   ~/.<agent>-shared/      what every account has in common
#   ~/.<agent>-profile-N/   account N: its own login, symlinks for the rest
#   ~/.<agent>              symlink to profile 1, so the bare command still works
#
#   profiles.sh init <claude|codex|gemini> [--dry-run]
#   profiles.sh add  <claude|codex|gemini> [N] [--dry-run]
#   profiles.sh aliases
#   profiles.sh doctor
#
# Every path derives from $HOME.  MARSPOT_PROFILES_SKIP_RUNNING_CHECK=1 skips
# the "is the agent running" check in init; it exists for the test suite, which
# runs against a scratch HOME while real agents are alive on the machine.

set -euo pipefail

DRY=0
PROBLEMS=0
BEGIN='# >>> marspot-profiles: derived aliases >>>'
END='# <<< marspot-profiles: derived aliases <<<'

die() {
  echo "profiles.sh: $*" >&2
  exit 1
}

usage() {
  sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
}

run() {
  if [ "$DRY" = 1 ]; then
    printf 'would run:'
    printf ' %q' "$@"
    printf '\n'
  else
    "$@"
  fi
}

exists() {
  [ -e "$1" ] || [ -L "$1" ]
}

# What is shared, per agent.  A trailing slash marks a directory, which is
# created in the shared dir so that every account links to it from the start;
# a file is linked once it exists.  Anything not listed belongs to one account.
#
# Shared on purpose: claude's history.jsonl, so that `--resume` finds a session
# whichever account started it.
# Not shared on purpose: gemini's projects.json.  gemini replaces it by rename,
# which turns a symlink back into a real file; each account keeps its own and
# finds the others' project directories through their .project_root markers.
agent_vars() {
  AGENT=$1
  SUB=""
  case "$AGENT" in
    claude)
      ENTRIES="CLAUDE.md settings.json history.jsonl keybindings.json agents/ commands/ file-history/ image-cache/ paste-cache/ plans/ projects/ rules/ session-env/ sessions/ shell-snapshots/ skills/ tasks/ telemetry/ plugins/blocklist.json plugins/cache/ plugins/marketplaces/"
      ACCOUNT_FILES=".claude.json"
      ENV_NAME=CLAUDE_CONFIG_DIR
      LOGIN="run claude%s, then /login"
      ;;
    codex)
      ENTRIES="AGENTS.md config.toml models_cache.json session_index.jsonl cache/ packages/ plugins/ rules/ sessions/ shell_snapshots/ skills/"
      ACCOUNT_FILES="auth.json history.jsonl"
      ENV_NAME=CODEX_HOME
      LOGIN="run 'codex%s login'"
      ;;
    gemini)
      # GEMINI_CLI_HOME names a home; the config dir is .gemini inside it
      SUB=".gemini"
      ENTRIES="GEMINI.md settings.json agents/ commands/ extensions/ history/ policies/ skills/ tmp/"
      ACCOUNT_FILES="oauth_creds.json google_accounts.json gemini-credentials.json mcp-oauth-tokens.json a2a-oauth-tokens.json installation_id projects.json"
      ENV_NAME=GEMINI_CLI_HOME
      LOGIN="run gemini%s and authenticate when it asks"
      ;;
    *) die "unknown agent '$AGENT' (expected claude, codex or gemini)" ;;
  esac
  SHARED="$HOME/.$AGENT-shared"
  DEFAULT="$HOME/.$AGENT"
}

profile_dir() {
  echo "$HOME/.$AGENT-profile-$1"
}

# the directory the agent reads its config from, for profile $1
cfg_dir() {
  echo "$HOME/.$AGENT-profile-$1${SUB:+/$SUB}"
}

is_shared_entry() {
  local e
  for e in $ENTRIES; do
    if [ "${e%/}" = "$1" ]; then
      return 0
    fi
  done
  return 1
}

# true when the shared dir has (or, in a dry run of init, would have) $1
shared_has() {
  if exists "$SHARED/$1"; then
    return 0
  fi
  [ "$DRY" = 1 ] && [ ! -L "$DEFAULT" ] && exists "$DEFAULT/$1"
}

# give config dir $1 a symlink for every shared entry it does not have yet
link_shared() {
  local cfg=$1 e name
  for e in $ENTRIES; do
    name=${e%/}
    if ! shared_has "$name"; then
      if [ "$e" = "$name" ]; then
        continue
      fi
      run mkdir -p "$SHARED/$name"
    fi
    if exists "$cfg/$name"; then
      continue
    fi
    if [ "$name" != "${name##*/}" ] && [ ! -d "$(dirname "$cfg/$name")" ]; then
      run mkdir -p "$(dirname "$cfg/$name")"
    fi
    run ln -s "$SHARED/$name" "$cfg/$name"
  done
}

refuse_if_running() {
  local pids
  if [ "${MARSPOT_PROFILES_SKIP_RUNNING_CHECK:-}" = 1 ] || [ "$DRY" = 1 ]; then
    return 0
  fi
  pids=$(pgrep -x "$AGENT" | tr '\n' ' ' || true)
  if [ -n "$pids" ]; then
    die "$AGENT is running (pid $pids); it writes into $DEFAULT, which is about to move. Quit every $AGENT session and background process, then run this again"
  fi
}

# a relative symlink would point somewhere else after the move
refuse_relative_link() {
  if [ -L "$1" ]; then
    case "$(readlink "$1")" in
      /*) ;;
      *) die "$1 is a relative symlink ($(readlink "$1")); make it absolute or remove it, then run this again" ;;
    esac
  fi
}

cmd_init() {
  agent_vars "$1"
  local cfg backup e name p
  cfg=$(cfg_dir 1)
  if [ -L "$DEFAULT" ]; then
    if [ "$(readlink "$DEFAULT")" != "$cfg" ]; then
      die "$DEFAULT is a symlink to $(readlink "$DEFAULT"), not to $cfg; leaving it alone"
    fi
    echo "$AGENT: already converted ($DEFAULT -> $cfg), nothing to do"
    return 0
  fi
  if [ ! -d "$DEFAULT" ]; then
    die "$DEFAULT does not exist; run $AGENT once so that it creates its directory, then run this again"
  fi
  refuse_if_running
  backup="$DEFAULT.backup-$(date +%Y%m%d-%H%M%S)"
  if exists "$backup"; then
    die "$backup already exists"
  fi
  for p in "$DEFAULT"/* "$DEFAULT"/.[!.]* "$DEFAULT"/..?*; do
    refuse_relative_link "$p"
  done
  for e in $ENTRIES; do
    refuse_relative_link "$DEFAULT/${e%/}"
  done

  run mkdir -p "$SHARED" "$cfg"

  for e in $ENTRIES; do
    name=${e%/}
    exists "$DEFAULT/$name" || continue
    if exists "$SHARED/$name"; then
      echo "not moved: $DEFAULT/$name ($SHARED/$name already exists)"
      continue
    fi
    if [ "$name" != "${name##*/}" ] && [ ! -d "$(dirname "$SHARED/$name")" ]; then
      run mkdir -p "$(dirname "$SHARED/$name")"
    fi
    run mv "$DEFAULT/$name" "$SHARED/$name"
  done

  # the rest is account 1's own
  for p in "$DEFAULT"/* "$DEFAULT"/.[!.]* "$DEFAULT"/..?*; do
    exists "$p" || continue
    name=${p##*/}
    if is_shared_entry "$name"; then
      continue
    fi
    if exists "$cfg/$name"; then
      echo "not moved: $p ($cfg/$name already exists)"
      continue
    fi
    run mv "$p" "$cfg/$name"
  done

  # without CLAUDE_CONFIG_DIR claude keeps this file in $HOME, not in ~/.claude.
  # Copied, not moved: the bare `claude` goes on reading the original.
  if [ "$AGENT" = claude ] && [ -f "$HOME/.claude.json" ] && [ ! -L "$HOME/.claude.json" ] \
    && ! exists "$cfg/.claude.json" && ! exists "$DEFAULT/.claude.json"; then
    run cp -p "$HOME/.claude.json" "$cfg/.claude.json"
  fi

  link_shared "$cfg"
  run mv "$DEFAULT" "$backup"
  run ln -s "$cfg" "$DEFAULT"

  echo "$AGENT: shared dir $SHARED, account 1 in $(profile_dir 1), $DEFAULT -> $cfg"
  echo "  what was not moved is in $backup (empty when everything moved)"
  case "$AGENT" in
    claude)
      echo "  claude1 is not logged in: the keychain entry is named after the config dir path and does not follow a move"
      echo "  open a new shell, run claude1 and /login once (the bare claude keeps its login)"
      ;;
    codex) echo "  auth.json moved with account 1, so codex1 and the bare codex stay logged in" ;;
    gemini) echo "  oauth_creds.json moved with account 1, so gemini1 and the bare gemini stay logged in" ;;
  esac
  echo "  next: profiles.sh aliases, then profiles.sh add $AGENT for each further account"
}

next_number() {
  local d n max=0
  for d in "$HOME/.$AGENT-profile-"*; do
    n=${d##*-}
    case "$n" in
      '' | *[!0-9]*) continue ;;
    esac
    if [ "$n" -gt "$max" ]; then
      max=$n
    fi
  done
  echo $((max + 1))
}

cmd_add() {
  agent_vars "$1"
  local n=${2:-} dir cfg
  if [ ! -d "$SHARED" ]; then
    die "$SHARED does not exist; run 'profiles.sh init $AGENT' first"
  fi
  if [ -z "$n" ]; then
    n=$(next_number)
  fi
  case "$n" in
    '' | 0* | *[!0-9]*) die "profile number must be a positive integer, got '$n'" ;;
  esac
  if [ "$n" -gt 255 ]; then
    die "profile number must be at most 255, got $n"
  fi
  dir=$(profile_dir "$n")
  cfg=$(cfg_dir "$n")
  if exists "$dir"; then
    die "$dir already exists"
  fi
  run mkdir -p "$cfg"
  link_shared "$cfg"
  echo "$AGENT: account $n in $dir (${ENV_NAME}=$dir)"
  echo "  it has no login yet, and none may be copied from another account"
  # shellcheck disable=SC2059
  printf "  open a new shell, $LOGIN\n" "$n"
}

zsh_block() {
  cat <<'BLOCK'
# >>> marspot-profiles: derived aliases >>>
# claudeN / codexN / geminiN exist for every ~/.<agent>-profile-N that exists.
# Creating the directory creates the alias in the next shell; nothing to edit.
for _mp_dir in "$HOME"/.claude-profile-*(N); do
  alias "claude${_mp_dir##*-}=CLAUDE_CONFIG_DIR='$_mp_dir' claude"
done
for _mp_dir in "$HOME"/.codex-profile-*(N); do
  alias "codex${_mp_dir##*-}=CODEX_HOME='$_mp_dir' codex"
done
for _mp_dir in "$HOME"/.gemini-profile-*(N); do
  alias "gemini${_mp_dir##*-}=GEMINI_CLI_HOME='$_mp_dir' gemini"
done
unset _mp_dir
# <<< marspot-profiles: derived aliases <<<
BLOCK
}

bash_block() {
  cat <<'BLOCK'
# >>> marspot-profiles: derived aliases >>>
# claudeN / codexN / geminiN exist for every ~/.<agent>-profile-N that exists.
# Creating the directory creates the alias in the next shell; nothing to edit.
_mp_nullglob=$(shopt -p nullglob)
shopt -s nullglob
for _mp_dir in "$HOME"/.claude-profile-*; do
  alias "claude${_mp_dir##*-}=CLAUDE_CONFIG_DIR='$_mp_dir' claude"
done
for _mp_dir in "$HOME"/.codex-profile-*; do
  alias "codex${_mp_dir##*-}=CODEX_HOME='$_mp_dir' codex"
done
for _mp_dir in "$HOME"/.gemini-profile-*; do
  alias "gemini${_mp_dir##*-}=GEMINI_CLI_HOME='$_mp_dir' gemini"
done
eval "$_mp_nullglob"
unset _mp_dir _mp_nullglob
# <<< marspot-profiles: derived aliases <<<
BLOCK
}

# put the block from file $2 into rc file $1, replacing the one already there
install_block() {
  local rc=$1 block=$2 new
  new=$(mktemp)
  if [ -f "$rc" ] && grep -qxF "$BEGIN" "$rc"; then
    if ! grep -qxF "$END" "$rc"; then
      die "$rc has the opening line of the alias block but not the closing one; repair it by hand"
    fi
    awk -v b="$BEGIN" -v e="$END" -v f="$block" '
      $0 == b { while ((getline line < f) > 0) print line; skip = 1; next }
      $0 == e { skip = 0; next }
      !skip { print }
    ' "$rc" >"$new"
  else
    if [ -s "$rc" ]; then
      cat "$rc" >"$new"
      echo >>"$new"
    fi
    cat "$block" >>"$new"
  fi
  if [ -f "$rc" ] && cmp -s "$new" "$rc"; then
    echo "aliases: $rc is up to date"
  else
    if [ -f "$rc" ] && [ ! -e "$rc.marspot-profiles.bak" ]; then
      cp -p "$rc" "$rc.marspot-profiles.bak"
      echo "aliases: saved $rc.marspot-profiles.bak"
    fi
    # written in place, so an rc that is a symlink stays one
    cat "$new" >"$rc"
    echo "aliases: wrote the alias block to $rc"
  fi
  rm -f "$new"
}

cmd_aliases() {
  local block
  block=$(mktemp)
  zsh_block >"$block"
  install_block "$HOME/.zshrc" "$block"
  if [ -f "$HOME/.bashrc" ]; then
    bash_block >"$block"
    install_block "$HOME/.bashrc" "$block"
  fi
  rm -f "$block"
  echo "  open a new shell for the aliases to appear"
}

problem() {
  echo "problem: $*"
  PROBLEMS=$((PROBLEMS + 1))
}

doctor_profile() {
  local n=$1 cfg e name link want f before=$PROBLEMS
  cfg=$(cfg_dir "$n")
  if [ ! -d "$cfg" ]; then
    problem "$cfg is not a directory"
    return 0
  fi
  for e in $ENTRIES; do
    name=${e%/}
    link="$cfg/$name"
    want="$SHARED/$name"
    if [ -L "$link" ]; then
      if [ "$(readlink "$link")" != "$want" ]; then
        problem "$link points to $(readlink "$link"), expected $want"
      elif [ ! -e "$link" ]; then
        problem "$link is a dangling symlink ($want does not exist)"
      fi
    elif [ -e "$link" ]; then
      problem "$link is not a symlink, so this account does not share it (expected -> $want)"
    elif exists "$want"; then
      problem "$link is missing (expected -> $want)"
    fi
  done
  for f in $ACCOUNT_FILES; do
    if [ -L "$cfg/$f" ]; then
      problem "$cfg/$f is a symlink; it must be a real file that belongs to this account only"
    fi
  done
  if [ "$PROBLEMS" = "$before" ]; then
    echo "ok: $(profile_dir "$n")"
  fi
}

# no pipeline anywhere on this path: a loop on the right of a pipe runs in a
# subshell, and the count of problems would stay there
doctor_agent() {
  local d n found=0
  for d in "$HOME/.$AGENT-profile-"*; do
    n=${d##*-}
    case "$n" in
      '' | *[!0-9]*) continue ;;
    esac
    exists "$d" || continue
    if [ ! -d "$SHARED" ]; then
      problem "$SHARED does not exist, but $d does"
      return 0
    fi
    found=1
    doctor_profile "$n"
  done
  if [ "$found" = 0 ]; then
    echo "ok: no $AGENT profiles"
  fi
}

cmd_doctor() {
  local a rc
  for a in claude codex gemini; do
    agent_vars "$a"
    doctor_agent
  done
  for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
    if [ "$rc" = "$HOME/.bashrc" ] && [ ! -f "$rc" ]; then
      continue
    fi
    if [ -f "$rc" ] && grep -qxF "$BEGIN" "$rc" && grep -qxF "$END" "$rc"; then
      echo "ok: alias block in $rc"
    else
      problem "no alias block in $rc (run: profiles.sh aliases)"
    fi
  done
  if [ "$PROBLEMS" != 0 ]; then
    echo "$PROBLEMS problem(s)"
    exit 1
  fi
  echo "all clear"
}

if [ -z "${HOME:-}" ] || [ ! -d "$HOME" ]; then
  die "HOME is not a directory: '${HOME:-}'"
fi

ARGS=()
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY=1 ;;
    -*) usage ;;
    *) ARGS+=("$arg") ;;
  esac
done
set -- ${ARGS[@]+"${ARGS[@]}"}

case "${1:-}" in
  init)
    [ $# = 2 ] || usage
    cmd_init "$2"
    ;;
  add)
    [ $# = 2 ] || [ $# = 3 ] || usage
    cmd_add "$2" "${3:-}"
    ;;
  aliases)
    [ $# = 1 ] && [ "$DRY" = 0 ] || usage
    cmd_aliases
    ;;
  doctor)
    [ $# = 1 ] && [ "$DRY" = 0 ] || usage
    cmd_doctor
    ;;
  *) usage ;;
esac
