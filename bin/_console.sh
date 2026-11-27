# shellcheck shell=bash
# Whether a login sees the console session: a plain ssh login of the
# console user reaches its window server and LaunchServices, and that is
# the only way in a runner allows, so a run that opens windows checks it
# first and stops when it fails.  Sourced by the scripts that send a
# window run to a runner and by the bench on the runner itself.

# the listing that names the Dock only in a login that sees the console
CONSOLE_LIST="perl -e 'alarm shift; exec @ARGV' 20 /usr/bin/lsappinfo list"

# console_check <who> <command...>: run <command> (one that prints
# `lsappinfo list`); return 0 when it lists the Dock, else say what ran,
# its exit code and the first lines it printed, and return 1
console_check() {
  local who=$1 out rc=0
  shift
  out="$("$@" </dev/null 2>&1)" || rc=$?
  grep -q 'path="/System/Library/CoreServices/Dock.app' <<<"$out" && return 0
  echo "FAIL: $who does not see the console session: \`$CONSOLE_LIST\` (exit $rc)" \
    "lists no Dock; is the same user logged in at the console? What it printed:" >&2
  printf '%s\n' "$out" | head -20 | sed 's/^/    /' >&2
  return 1
}
