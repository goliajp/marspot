# shellcheck shell=bash
# Sourced by every script that would start a real agent CLI (claude,
# codex, gemini, kimi): such a CLI runs on the machine someone works at
# and never on a runner.  Do not execute.
#
# A machine is a runner when any of these holds:
#   - /usr/local/bin/bench-lock exists: the runners' shared lock, which
#     every runner has and the interactive machine does not;
#   - the tree the script runs from is a remote copy: remote-sync writes
#     `.remote-sync` at the top of every copy it makes on a runner;
#   - the short host name is one of MARSPOT_RUNNER_HOSTS.
# MARSPOT_RUNNER_MARKERS replaces the two marker paths, for the guard's
# own test.

MARSPOT_RUNNER_HOSTS="${MARSPOT_RUNNER_HOSTS:-mini mini2 mini3 lx64 firefly}"

# runner_reason: why this machine is a runner, on stdout; status 1 when
# it is not one
runner_reason() {
  local root markers m host
  root="${ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
  markers="${MARSPOT_RUNNER_MARKERS-/usr/local/bin/bench-lock $root/.remote-sync}"
  for m in $markers; do
    [[ -e $m ]] && { echo "$m exists"; return 0; }
  done
  host="$(hostname -s 2>/dev/null)"
  [[ " $MARSPOT_RUNNER_HOSTS " == *" $host "* ]] && { echo "the host is $host"; return 0; }
  return 1
}

# refuse_real_agent_on_runner WHAT: exits 2 on a runner, naming WHAT
refuse_real_agent_on_runner() {
  local why
  why="$(runner_reason)" || return 0
  echo "refusing to start ${1:-a real agent CLI} on a runner ($why): real agent CLIs run only on the interactive machine" >&2
  exit 2
}

# is_runner_host NAME: whether NAME is a runner, for a script that starts
# the CLI on another host
is_runner_host() {
  [[ " $MARSPOT_RUNNER_HOSTS " == *" ${1%%.*} "* ]]
}
