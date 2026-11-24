#!/usr/bin/env bash
#
# Helpers with real agents and real accounts, against the installed app.
#
# For each agent asked for it starts, under a shadow shell pane of its
# own, one helper on the host and one in a container, each given a task
# that needs one line back, and checks that the line reaches the result
# file and that the helper is seen to stop.  For the host claude helper,
# started with --model, it checks that the account switching recognises
# the model and effort it runs with.  Once a host helper is idle at its
# prompt it samples the resident memory of the processes under the
# helper's pane, and for a container helper the container's memory as
# docker reports it; one `MEM …` line per helper.
#
# Real accounts are used, so each run costs a little quota: the tasks
# are one line and claude runs on haiku.  It talks to the running app's
# control socket and opens shadow panes only: no window, no sandbox,
# none of the panes already open is touched.  Run it after
# bin/install-local.sh, on the machine whose accounts it is to use; never
# on a runner.
#
#   --agents claude,codex      the agents (default: all four)
#   --memory-only              start host helpers only and measure them;
#                              their tasks are not checked (an agent with
#                              no working login idles at its login screen)
#   --no-container             host helpers only
#   --cwd DIR                  the helpers' directory (default: a new one)
#   --under PANE               the pane its shadow shell opens under
#                              (default: the pane it runs in; needed
#                              when it runs outside the panes)
#
# Every step has a time limit.  Every helper and pane it started is
# stopped at the end, also on failure.  Exits non-zero when any check
# failed or any helper could not be measured.

set -uo pipefail

AGENTS="claude,codex,gemini,kimi"
MEMORY_ONLY=0 CONTAINER=1 CWD="" UNDER=""
while (($#)); do
  case "$1" in
    --agents) AGENTS="$2"; shift 2 ;;
    --memory-only) MEMORY_ONLY=1; shift ;;
    --no-container) CONTAINER=0; shift ;;
    --cwd) CWD="$2"; shift 2 ;;
    --under) UNDER="$2"; shift 2 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
((MEMORY_ONLY)) && CONTAINER=0

if [[ -n "${MARSPOT_STATE_DIR:-}" ]]; then
  echo "MARSPOT_STATE_DIR is set: this script is for the installed app" >&2
  exit 2
fi
PEER="$HOME/.local/bin/marspot-peer"
STATE="$HOME/Library/Caches/marspot"
APPLOG="$HOME/Library/Logs/Marspot/marspot.log"
[[ -x "$PEER" ]] || { echo "no $PEER: run bin/install-local.sh first" >&2; exit 2; }
[[ -S "$STATE/control.sock" ]] || { echo "the installed app is not running ($STATE/control.sock)" >&2; exit 2; }

OWN="$(mktemp -d /tmp/marspot-helpers-real.XXXXXX)" || exit 1
[[ -n "$CWD" ]] || { CWD="$OWN/work"; mkdir -p "$CWD"; }
CWD="$(cd "$CWD" && pwd -P)"
failures=0
STARTED=()   # helper ids, stopped at the end
PARENT=""
ROWS=()      # summary rows

peer() { timeout "${T:-30}" "$PEER" "$@"; }
say() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*"; }
fail() { say "FAIL: $*"; failures=$((failures + 1)); }
json() { python3 -I -c 'import json,sys
v=json.load(sys.stdin)
for k in sys.argv[1].split("."):
    v=v[int(k)] if isinstance(v,list) else (v or {}).get(k)
print("" if v is None else v)' "$1"; }

cleanup() {
  local h
  for h in ${STARTED[@]+"${STARTED[@]}"}; do
    [[ -n "$h" ]] || continue
    T=60 peer helper stop "$h" >/dev/null 2>&1 || say "could not stop $h"
    timeout 30 docker ps -q --filter "label=marspot.helper=$h" 2>/dev/null | xargs -r timeout 60 docker rm -f >/dev/null 2>&1
  done
  [[ -n "$PARENT" ]] && T=30 peer pane close "$PARENT" >/dev/null 2>&1
  rm -rf "${OWN:?}"
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP

# the state helper `$1` (its id) is in, from `helper list --all`
state_of() {
  T=20 peer helper list --all --json | python3 -I -c 'import json,sys
for h in json.load(sys.stdin)["result"]["helpers"]:
    if h["id"] == int(sys.argv[1]): print(h["state"]); break' "$1"
}

# the shell pid of session `$1`
shell_of() { sed -n 's/^shell_child_pid = //p' "$STATE/sessions/$1/entry.toml" 2>/dev/null; }

# total RSS in KiB of the processes under pid `$1` (not itself), the
# number of them and the largest one's command
tree_rss() {
  ps -A -o pid=,ppid=,rss=,comm= | python3 -I -c 'import sys
rows=[l.split(None,3) for l in sys.stdin if l.strip()]
kids={}
for p,pp,r,c in rows: kids.setdefault(pp,[]).append((p,int(r),c))
out=[]; todo=[sys.argv[1]]
while todo:
    for k in kids.get(todo.pop(),[]): out.append(k); todo.append(k[0])
big=max(out,key=lambda k:k[1])[2].rsplit("/",1)[-1] if out else "-"
print(sum(k[1] for k in out), len(out), big)' "$1"
}

# codex's TUI talks to an app-server daemon outside the pane, shared by
# the panes of an account: the daemons' total RSS in KiB and their count
# (read only, to compare before and after a helper)
app_servers() {
  ps -A -o rss=,command= | awk '/bin\/codex( app-server|-code-mode-host)/ {s += $1; n++} END {print s + 0, n + 0}'
}

median() { tr ' ' '\n' | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }

# start a helper: agent, isolation flag (or ""), task; prints the reply
start() {
  local agent="$1" iso="$2" task="$3" model=()
  [[ "$agent" == claude ]] && model=(--model haiku)
  T=120 peer helper start --json --agent "$agent" --parent "$PARENT" --cwd "$CWD" --budget 15 \
    --name "real-$agent${iso:+-box}" ${model[@]+"${model[@]}"} $iso "$task"
}

# wait up to `$2` seconds for helper `$1` (id)'s result file `$3` to
# hold `$4`.  Its stop is not waited for: the stop's notice goes to the
# parent pane's inbox, a shell parent has none, and the helper stays
# `running` in the list until it is delivered.  The pane going quiet
# before it is measured stands for the stop.
await_result() {
  local sid="$1" secs="$2" file="$3" token="$4" end=$((SECONDS + $2)) st=""
  while ((SECONDS < end)); do
    grep -qF "$token" "$file" 2>/dev/null && return 0
    st="$(state_of "$sid")"
    [[ -z "$st" ]] && { say "helper $sid is gone from the helper list"; return 1; }
    sleep 3
  done
  say "helper $sid after ${secs}s: state=$st, result: $(head -c 200 "$file" 2>/dev/null || echo none)"
  return 1
}

# wait up to `$2` s for pane `$1` to print nothing for `$3` s
await_quiet() {
  local end=$((SECONDS + $2))
  while ((SECONDS < end)); do
    T=20 peer helper list --all --json | python3 -I -c 'import json,sys
for h in json.load(sys.stdin)["result"]["helpers"]:
    if h["id"] == int(sys.argv[1]) and (h["quiet_s"] or 0) >= int(sys.argv[2]): sys.exit(0)
sys.exit(1)' "$1" "$3" && return 0
    sleep 2
  done
  return 1
}

# three samples of the agent's tree under host helper `$2` (id `$3`)
measure_host() {
  local agent="$1" name="$2" sid="$3" shell s samples=() n=0 big=-
  shell="$(shell_of "$sid")"
  [[ -n "$shell" ]] || { fail "$agent: no shell pid for session $sid"; return 1; }
  await_quiet "$sid" 120 10 || say "$agent: pane $sid never quiet for 10 s; measuring anyway"
  for _ in 1 2 3; do
    read -r s n big < <(tree_rss "$shell")
    samples+=("$s"); sleep 2
  done
  if ((n == 0)); then
    fail "$agent: nothing runs under pane $sid's shell"; return 1
  fi
  local med; med="$(echo "${samples[*]}" | median)"
  echo "MEM agent=$agent isolation=host sid=$sid procs=$n largest=$big rss_kib=$(IFS=,; echo "${samples[*]}") median_kib=$med"
  ROWS+=("$agent host mem $med KiB ($n procs, largest $big)")
  T=20 peer pane read "$sid" -n 12 > "$OWN/screen-$agent" 2>&1
  say "$agent: its screen while idle (last lines):"; sed 's/^/    /' "$OWN/screen-$agent"
}

# the container's memory as docker reports it, and the docker CLI's tree
measure_box() {
  local agent="$1" sid="$2" shell mem cli id samples=()
  shell="$(shell_of "$sid")"
  await_quiet "$sid" 120 10 || say "$agent box: pane $sid never quiet for 10 s; measuring anyway"
  id="$(timeout 30 docker ps -q --filter "label=marspot.helper=$sid" | head -1)"
  [[ -n "$id" ]] || { fail "$agent box: no running container labelled marspot.helper=$sid"; return 1; }
  for _ in 1 2 3; do
    samples+=("$(timeout 30 docker stats --no-stream --format '{{.MemUsage}}' "$id" | cut -d/ -f1 | tr -d ' ')")
    sleep 2
  done
  mem="${samples[*]}"
  cli="$(tree_rss "${shell:-0}" | cut -d' ' -f1)"
  [[ -n "${mem// }" ]] || { fail "$agent box: docker reports no memory for container $id"; return 1; }
  echo "MEM agent=$agent isolation=container sid=$sid container_mem=${mem// /,} host_cli_kib=$cli"
  ROWS+=("$agent container mem $(median <<< "${samples[*]}") in the container, $cli KiB of docker CLI on the host")
}

# whether the account switching takes claude helper `$1` into account:
# its model and effort confirmed, as confirm_choice in the claudecode
# plugin does.  The plugin logs `quota.unjudged` for a pane it leaves
# out and `quota.judged` when it takes it in again, and writes a
# `rebalance.*` line only for a pane it takes in.  Read from log line
# `$2` on, until the last of these lines about the pane is not
# `unjudged`.
check_switchable() {
  local sid="$1" from="$2" end=$((SECONDS + 120)) last="" re
  re="plugin\.claudecode\.(quota\.(un)?judged|rebalance\.[a-z_]+)"$'\t'"sid=$sid "
  while ((SECONDS < end)); do
    last="$(tail -n +"$from" "$APPLOG" | grep -aE "$re" | tail -1 | cut -f7-)"
    [[ -n "$last" && "$last" != *quota.unjudged* ]] && break
    sleep 3
  done
  echo "SWITCH sid=$sid last=${last:-none}"
  if [[ -z "$last" || "$last" == *quota.unjudged* ]]; then
    fail "claude helper $sid is not taken into account switching: ${last:-no line about it}"; return 1
  fi
  ROWS+=("claude host taken into switching: ${last//$'\t'/ }")
}

# one helper end to end; `$2` is "" or --container
run_one() {
  local agent="$1" iso="$2" kind=host token reply name sid file acct from
  [[ -n "$iso" ]] && kind=container
  token="HELPERS-REAL-$agent-$kind-$RANDOM$RANDOM"
  from=$(($(wc -l < "$APPLOG") + 1))
  local servers_before; servers_before="$(app_servers)"
  say "$agent $kind: starting"
  reply="$(start "$agent" "$iso" "Write one line holding the word $token into the result file named above, then end your turn. Change no other file.")" \
    || { fail "$agent $kind: helper start: $reply"; ROWS+=("$agent $kind START FAILED: $(json error <<< "$reply" 2>/dev/null)"); return; }
  [[ "$(json ok <<< "$reply")" == True ]] || { fail "$agent $kind: $reply"; ROWS+=("$agent $kind REFUSED: $(json error <<< "$reply")"); return; }
  name="$(json result.name <<< "$reply")" sid="$(json result.id <<< "$reply")"
  file="$(json result.result_path <<< "$reply")" acct="$(json result.account <<< "$reply")"
  STARTED+=("$sid")
  say "$agent $kind: $name (id $sid) on $acct, model $(json result.model <<< "$reply"), $(json result.state <<< "$reply")"
  if ((MEMORY_ONLY)); then
    sleep 20
  elif await_result "$sid" "$([[ $kind == container ]] && echo 1200 || echo 300)" "$file" "$token"; then
    ROWS+=("$agent $kind result arrived")
    say "$agent $kind: result arrived"
  else
    fail "$agent $kind: no result"; ROWS+=("$agent $kind NO RESULT")
  fi
  if [[ $kind == host ]]; then
    measure_host "$agent" "$name" "$sid"
    if [[ $agent == codex ]]; then
      local after; after="$(app_servers)"
      echo "MEM agent=codex isolation=host part=app-server kib_and_count_before=${servers_before// /,} after=${after// /,}"
      [[ "${after#* }" != 0 ]] || fail "codex host: no app-server daemon found to measure"
      ROWS+=("codex host app-server daemons (outside the pane, shared): ${servers_before% *} -> ${after% *} KiB")
    fi
    [[ $agent == claude ]] && ((!MEMORY_ONLY)) && check_switchable "$sid" "$from"
  else
    measure_box "$agent" "$sid"
  fi
  if T=60 peer helper stop "$sid" >/dev/null; then
    local kept=() h
    for h in ${STARTED[@]+"${STARTED[@]}"}; do [[ "$h" == "$sid" ]] || kept+=("$h"); done
    STARTED=(${kept[@]+"${kept[@]}"})
  fi
}

reply="$(T=30 peer pane open --json --cwd "$CWD" ${UNDER:+--parent "$UNDER"})" || { echo "pane open failed: $reply"; exit 1; }
# by id: a pane opened a moment ago is not known by name yet
PARENT="$(json result.id <<< "$reply")"
[[ -n "$PARENT" ]] || { echo "pane open: $reply"; exit 1; }
say "parent pane $PARENT in $CWD"
for agent in ${AGENTS//,/ }; do
  run_one "$agent" ""
  ((CONTAINER)) && run_one "$agent" "--container"
done

echo
echo "summary ($(date '+%F %T'), agents $AGENTS$([[ $MEMORY_ONLY == 1 ]] && echo ', memory only')):"
printf '  %s\n' ${ROWS[@]+"${ROWS[@]}"}
echo "failures: $failures"
((failures == 0))
