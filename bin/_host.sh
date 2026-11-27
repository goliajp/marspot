# shellcheck shell=bash
# Which runner a job goes to.  Sourced by bin/_lib.sh; do not execute.
#
# marspot_host <kind> [copy-name] prints one host.  Kinds: test (the test
# suite, the CI mac job), gui (anything that opens a window or launches
# the app) and bench (every measurement).  MARSPOT_TEST_HOST,
# MARSPOT_GUI_HOST and MARSPOT_BENCH_HOST, when set, win outright.
# Otherwise `devops runner pick` decides, told the remote copy's name so a
# runner already holding a warm copy is preferred.  When devops is
# missing, fails, or answers with a machine this project does not use,
# the named fallback is used and one line on stderr says why.
#
# Call it once per run and keep the answer: the sync, the run and the
# fetch-back have to reach the same machine, and a second pick may not.
_MARSPOT_HOST_FALLBACK_test=mini2
_MARSPOT_HOST_FALLBACK_gui=mini2
_MARSPOT_HOST_FALLBACK_bench=mini3
# runners devops may still offer that this project no longer sends work to
MARSPOT_RUNNER_SKIP="${MARSPOT_RUNNER_SKIP:-mini}"

# marspot_host_named <kind>: the override or the fallback, without asking
# devops
marspot_host_named() {
  local fb var
  case "${1:-}" in
    test|gui|bench) ;;
    *) echo "marspot_host: test|gui|bench, not '${1:-}'" >&2; return 2 ;;
  esac
  fb="_MARSPOT_HOST_FALLBACK_$1"
  var="MARSPOT_$(tr '[:lower:]' '[:upper:]' <<<"$1")_HOST"
  echo "${!var:-${!fb}}"
}

# _marspot_pick_parse <json on stdin>: `<device> <verdict>`, the verdict
# being `ok` when devops scores the device as eligible for the kind, else
# its reason for excluding it.  The answer comes from another process, so
# a malformed one is an error, not a host.
_marspot_pick_parse() {
  python3 -c '
import json, re, sys
d = json.load(sys.stdin)
dev = d.get("device")
if not isinstance(dev, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", dev):
    sys.exit("no usable device in the answer")
row = next((r for r in d.get("scored") or [] if r.get("device") == dev), None)
why = "not scored" if row is None else row.get("excluded")
print(dev, "ok" if why is None else str(why).replace(" ", "_"))
'
}

marspot_host() {
  local kind=${1:-} repo=${2:-} var fb out dev verdict why=""
  marspot_host_named "$kind" >/dev/null || return 2
  var="MARSPOT_$(tr '[:lower:]' '[:upper:]' <<<"$kind")_HOST"
  if [[ -n ${!var:-} ]]; then
    echo "${!var}"
    return 0
  fi
  fb="_MARSPOT_HOST_FALLBACK_$kind"
  fb=${!fb}
  if ! command -v devops >/dev/null 2>&1; then
    why="devops is not on PATH"
  elif ! out=$("$ROOT/bin/_bounded.sh" 10 devops runner pick --kind "$kind" \
      --platform macos-arm64 ${repo:+--repo "$repo"} --json </dev/null 2>/dev/null); then
    why="devops runner pick failed or did not answer within 10 s"
  elif ! read -r dev verdict < <(_marspot_pick_parse <<<"$out" 2>/dev/null) || [[ -z $dev ]]; then
    why="devops runner pick gave an answer that is not a host"
  elif [[ " $MARSPOT_RUNNER_SKIP " == *" $dev "* ]]; then
    why="devops picked $dev, which this project no longer uses"
  elif [[ $dev != "$fb" && $verdict != ok ]]; then
    why="devops picked $dev but does not score it as a $kind runner ($verdict)"
  fi
  if [[ -n $why ]]; then
    echo "==> runner for $kind: $fb, the fallback: $why" >&2
    echo "$fb"
    return 0
  fi
  echo "==> runner for $kind: $dev (devops runner pick)" >&2
  echo "$dev"
}
