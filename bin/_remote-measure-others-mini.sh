#!/usr/bin/env bash
# bin/_remote-measure-others-mini.sh — runs on the bench host, driven
# from bin/remote-measure-others.sh. Emits a single JSON object on
# stdout shaped like:
#
#   {
#     "terminal": {"cat-ascii_MBps": 12.3, ...},
#     "iterm":    {...},
#     "warp":     {...}
#   }
#
# Direct invocation from a dev machine is supported but unusual — the
# canonical entry point is bin/remote-measure-others.sh on the
# triggering host.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

SCENARIOS=(cat-ascii cat-mixed cat-cjk cat-emoji)
TRIALS=3
# Console mode drives all four terminals. ssh mode falls back to
# ghostty-only (see ASUSER block below). Terminal.app onboarding is
# the one-time Automation grant during install-bench-launchagent.sh
# first-fire, after which subsequent LaunchAgent-spawned runs reuse
# the cached TCC scope.
# Measurement order matters: marspot in the SAME cycle as every
# competitor so all five are measured under identical mini load. The
# cycle is sequential — at any moment exactly one terminal is doing
# PTY work — so per-cell contention is zero. This is what makes the
# vs-best-other ratio fair: every cell is the terminal vs an idle box.
# (cooldown between terminals lets caches / scheduler settle.)
TERMS=(iterm warp ghostty terminal marspot)
# 4 scenarios × 3 trials × ~2 MB/s worst case = ~96 s budget; cap at
# 120 s so a single-terminal failure (Warp keystroke mangle, paste
# warning, frozen surface) doesn't hang the whole refresh for 10 min
# of wait-loop dead time. Terminals that don't produce a marker by
# then are skipped — their previous-day snapshot stays via per-
# terminal merge in remote-measure-others.sh.
MARKER_TIMEOUT_S=120
# Seconds between one terminal's close and the next one's launch.
# Lets mini cool: GPU thermal, scheduler-cached affinity, page-cache
# warmth for /tmp scenarios. 3 s is empirically enough for variance
# to settle to ~5 % run-to-run.
COOLDOWN_S=3

# ssh→GUI bridge. Direct ssh dispatch can't reach Aqua / WindowServer.
# After exhaustive testing on macOS 26.5 / iTerm 3.6.11 / Ghostty 1.3.1
# the only terminal we can drive automatically via ssh is **Ghostty**,
# using `sudo -n launchctl asuser <uid>` to inject the spawn into the
# user's GUI launchd domain so NSApp init + Metal surface bootstrap.
# Requires a one-time NOPASSWD sudoers entry:
#   doracawl ALL=(root) NOPASSWD: /bin/launchctl asuser <uid> *
#
# Why the other terminals can't be driven from ssh:
#  * iTerm 3.6.11 AppleScript dispatch fails under both `launchctl
#    bsexec gui/<uid>` (-1728 "Can't get application iTerm" — iTerm's
#    AE entry is session-scoped, ssh-launched instances don't register
#    where bsexec-spawned osascript can find them) and `sudo -n
#    launchctl asuser` (-1712 timeout — root osascript needs a separate
#    TCC Automation grant that can't be granted non-interactively).
#    AS syntax also varies between versions — `create window with
#    profile "Default"` errors at parse time in 3.6.11 even when iTerm
#    is fully running.
#  * Warp's driver depends on `tell application "System Events" to
#    keystroke`. SE keystroke is a TCC Accessibility-gated path that
#    is unreachable from any ssh-spawned osascript regardless of
#    bridge (-1712 in both).
#
# Result: iTerm / Warp / Terminal.app baseline data must be refreshed
# from a console (Screen Sharing) session via direct `bash
# bin/_remote-measure-others-mini.sh` with no SSH_CONNECTION env. Per-
# terminal merge keeps their previous snapshot intact when this ssh
# run produces no entry for them.
ASUSER=""
SSH_MODE=0
if [[ -n "${SSH_CONNECTION:-}" ]]; then
  SSH_MODE=1
  uid="$(id -u)"
  if sudo -n launchctl asuser "$uid" /usr/bin/true 2>/dev/null; then
    ASUSER="sudo -n launchctl asuser $uid"
  fi
  if [[ -z "$ASUSER" ]]; then
    echo "==> ssh mode: NOPASSWD launchctl asuser not configured — even Ghostty will fail. Add to /etc/sudoers.d/marspot-bench:" >&2
    echo "    doracawl ALL=(root) NOPASSWD: /bin/launchctl asuser $uid *" >&2
  else
    echo "==> ssh mode: ASUSER=on. Driving Ghostty only; iTerm/Warp/Terminal need console refresh." >&2
  fi
fi

# Make sure scenarios are present. gen-scenarios.sh writes the .bin
# files this script's cat targets read.
if [[ ! -f bench/scenarios/cat-ascii.bin ]]; then
  echo "==> generating scenarios" >&2
  ./bin/gen-scenarios.sh >&2
fi

build_one_cmd() {
  local marker=$1
  local cmd="rm -f $marker; "
  local s t
  for s in "${SCENARIOS[@]}"; do
    for ((t=1; t<=TRIALS; t++)); do
      cmd+="echo '==SCN== $s' >> $marker; "
      cmd+="/usr/bin/time -p /bin/cat $ROOT/bench/scenarios/$s.bin 2>> $marker; "
    done
  done
  cmd+="echo '==ALL_DONE==' >> $marker; "
  cmd+="exit"
  echo "$cmd"
}

# ---- pre-flight inventory + trap cleanup ----------------------------
# Test must not leave residue. Inventory records what state each app
# was in BEFORE we touched it, and the cleanup trap uses that to undo
# only what we created. Three categories of state are tracked:
#
#   - iTerm / Terminal.app windows: AppleScript-trackable by integer
#     id. Drivers return the ids of windows they create; trap closes
#     exactly those (no collateral close of the user's other work).
#   - Warp / Ghostty processes: no AppleScript dictionary, no per-
#     window handle. We record whether the app was running before; if
#     NOT, the trap quits the app entirely (we are the only cause).
#     If yes, we leave it alone — the user had a session.
#   - Temp wrapper / marker files: always cleaned at exit.
SESSION_DIR="/tmp/marspot-bench-session-$$"
mkdir -p "$SESSION_DIR"

# Sweep orphan session dirs from prior runs that were SIGKILL'd before
# their EXIT trap fired (parent killed -9, host rebooted mid-run, etc).
# A dir whose pid is no longer running can be safely removed — bench
# wrapper / marker leaks from that aborted run are addressed below in
# the normal cleanup paths.
for stale in /tmp/marspot-bench-session-*; do
  [[ -d "$stale" ]] || continue
  pid=${stale##*-}
  [[ "$pid" == "$$" ]] && continue
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "==> pre-flight: sweeping orphan session dir $stale (pid $pid dead)" >&2
    rm -rf "$stale" 2>/dev/null || true
  fi
done

inventory_pid_running() {
  # 1 if any non-grep process matches; 0 otherwise.
  pgrep -f "$1" >/dev/null 2>&1 && echo 1 || echo 0
}

# Snapshot pre-existing state.
PRE_ITERM=$(inventory_pid_running "iTerm.app/Contents/MacOS")
PRE_TERMINAL=$(inventory_pid_running "Terminal.app/Contents/MacOS/Terminal")
PRE_WARP=$(inventory_pid_running "Warp.app/Contents/MacOS/stable")
PRE_GHOSTTY=$(inventory_pid_running "Ghostty.app/Contents/MacOS/ghostty")
printf 'PRE_ITERM=%s\nPRE_TERMINAL=%s\nPRE_WARP=%s\nPRE_GHOSTTY=%s\n' \
  "$PRE_ITERM" "$PRE_TERMINAL" "$PRE_WARP" "$PRE_GHOSTTY" \
  > "$SESSION_DIR/inventory"
echo "==> pre-flight: iterm=$PRE_ITERM terminal=$PRE_TERMINAL warp=$PRE_WARP ghostty=$PRE_GHOSTTY" >&2

# macOS ships bash 3.2 → no `declare -A`. Use one var per terminal.
WIN_iterm=""
WIN_warp=""
WIN_ghostty=""
WIN_terminal=""
LAUNCHED_TERMS=()

quit_app_if_we_spawned() {
  # $1=app-display-name, $2=ps-pattern, $3=PRE_<APP> snapshot value.
  # If app was NOT running before our test, quit it now. If it was
  # already up, leave it (user had a session).
  local name=$1 pat=$2 was_running=$3
  if [[ "$was_running" == "0" ]]; then
    # Polite AE quit first — works for Cocoa apps even without an
    # AS dictionary (NSApplication respects the standard quit event).
    osascript -e "tell application \"$name\" to quit" 2>/dev/null || true
    sleep 0.5
    # Backstop for processes that ignore the AE (Ghostty 1.x sometimes
    # does). Match the binary path to avoid friendly fire on similarly-
    # named user processes.
    pkill -f "$pat" 2>/dev/null || true
    echo "==> cleanup: quit $name (we spawned it)" >&2
  else
    echo "==> cleanup: leaving $name running (was up before this run)" >&2
  fi
}

cleanup() {
  local rc=$?
  # Disarm the trap before doing any work so a fail inside cleanup
  # doesn't re-enter via set -e (we'd recurse forever). Also drop -e
  # so partial cleanup is preferred over an early abort.
  trap - EXIT INT TERM
  set +e

  # iTerm / Terminal: precise close by the ids we recorded — leaves the
  # user's other windows untouched even if both were already running.
  [[ -n "$WIN_iterm" ]]    && bin/drivers/iterm.sh    close-windows "$WIN_iterm"    >/dev/null 2>&1 || true
  [[ -n "$WIN_terminal" ]] && bin/drivers/terminal.sh close-windows "$WIN_terminal" >/dev/null 2>&1 || true

  # If the app wasn't running pre-flight, we caused the whole launch —
  # quit the app entirely so the process count returns to zero. If it
  # was running, we already closed only our windows above and leave the
  # rest of the user's session alone.
  quit_app_if_we_spawned "iTerm"    "iTerm.app/Contents/MacOS"              "$PRE_ITERM"
  quit_app_if_we_spawned "Terminal" "Terminal.app/Contents/MacOS/Terminal"  "$PRE_TERMINAL"
  quit_app_if_we_spawned "Warp"     "Warp.app/Contents/MacOS/stable"        "$PRE_WARP"
  quit_app_if_we_spawned "Ghostty"  "Ghostty.app/Contents/MacOS/ghostty"    "$PRE_GHOSTTY"

  # Always-clean residue: driver wrappers + scenario markers. Markers
  # may be root-owned from a sudo-asuser-driven ghostty run; the rm
  # falls through to sudo -n if NOPASSWD is configured, else best-
  # effort plain rm.
  rm -f /tmp/iterm-wrapper-* /tmp/terminal-wrapper-* /tmp/ghostty-wrapper-* /tmp/warp-wrapper-* 2>/dev/null || true
  rm -f /tmp/measure-*-all.txt 2>/dev/null || sudo -n rm -f /tmp/measure-*-all.txt 2>/dev/null || true
  rm -rf "$SESSION_DIR" 2>/dev/null || true
  echo "==> cleanup done (script rc=$rc)" >&2
  exit "$rc"
}
trap cleanup EXIT INT TERM

# Per-terminal close — runs immediately after that terminal's marker
# completes (or times out). Closes only this terminal; the others
# haven't been launched yet thanks to the sequential cycle below. The
# same pre=1 → leave-running rule as the EXIT trap applies.
close_one() {
  local t=$1
  # Use `if` blocks instead of `[[ test ]] && { block }` — the latter
  # returns false when the test is false and trips `set -e` at the end
  # of the function, killing the script mid-cycle. `if` short-circuits
  # cleanly with no failing exit code.
  case "$t" in
    iterm)
      if [[ -n "$WIN_iterm" ]]; then
        bin/drivers/iterm.sh close-windows "$WIN_iterm" >/dev/null 2>&1 || true
      fi
      if [[ "$PRE_ITERM" == "0" ]]; then
        # iTerm's "Prompt before quitting" preference can block an AS
        # quit indefinitely waiting for a Confirm dialog the receiver
        # can't answer. Polite AS quit first (clean teardown if the
        # prompt is disabled), pkill backstop within 1 s otherwise.
        osascript -e 'tell application "iTerm" to quit saving no' >/dev/null 2>&1 &
        sleep 1
        pkill -f "iTerm.app/Contents/MacOS/iTerm2" 2>/dev/null || true
      fi
      ;;
    terminal)
      if [[ -n "$WIN_terminal" ]]; then
        bin/drivers/terminal.sh close-windows "$WIN_terminal" >/dev/null 2>&1 || true
      fi
      if [[ "$PRE_TERMINAL" == "0" ]]; then
        # Same Confirm-dialog risk as iTerm.
        osascript -e 'tell application "Terminal" to quit' >/dev/null 2>&1 &
        sleep 1
        pkill -f "Terminal.app/Contents/MacOS/Terminal" 2>/dev/null || true
      fi
      ;;
    warp)
      if [[ "$PRE_WARP" == "0" ]]; then
        osascript -e 'tell application "Warp" to quit' >/dev/null 2>&1 || true
        sleep 0.5
        pkill -f "Warp.app/Contents/MacOS/stable" 2>/dev/null || true
      fi
      ;;
    ghostty)
      if [[ "$PRE_GHOSTTY" == "0" ]]; then
        osascript -e 'tell application "Ghostty" to quit' >/dev/null 2>&1 || true
        sleep 0.5
        pkill -f "Ghostty.app/Contents/MacOS/ghostty" 2>/dev/null || true
      fi
      ;;
    marspot)
      # mcli exits when its single session finishes (wrapper ends with
      # `exit`). pkill is belt-and-suspenders for a hung wrapper.
      pkill -x mcli 2>/dev/null || true
      pkill -x marspot 2>/dev/null || true
      ;;
  esac
  return 0
}

# Single sequential cycle: launch → wait → close → cooldown. At any
# moment exactly one terminal is doing PTY work, so per-cell CPU / IO
# contention is zero. Every cell is the terminal vs an idle mini —
# the only condition under which vs-best-other ratios are honest.
for t in "${TERMS[@]}"; do
  # In ssh mode, only Ghostty + marspot can be driven directly (Ghostty
  # via sudo asuser, marspot via direct binary). The other three need
  # the LaunchAgent route for ssh-from-dev-box driving.
  if [[ $SSH_MODE -eq 1 && "$t" != "ghostty" && "$t" != "marspot" ]]; then
    echo "==> [${t}] skip (ssh mode — use LaunchAgent trigger for full refresh)" >&2
    continue
  fi

  marker="/tmp/measure-${t}-all.txt"
  rm -f "$marker" 2>/dev/null || sudo -n rm -f "$marker" 2>/dev/null || true
  cmd="$(build_one_cmd "$marker")"

  echo "==> [${t}] launch" >&2
  launch_rc=0
  case "$t" in
    iterm)    WIN_iterm="$(bin/drivers/iterm.sh run-tabs 1 "$cmd" 2>&1)" || launch_rc=$? ;;
    warp)     bin/drivers/warp.sh run-single "$cmd" || launch_rc=$? ;;
    ghostty)  $ASUSER bin/drivers/ghostty.sh run-single "$cmd" || launch_rc=$? ;;
    terminal) WIN_terminal="$(bin/drivers/terminal.sh run-single "$cmd" 2>&1)" || launch_rc=$? ;;
    marspot)
      # marspot.sh expects an executable script as MARSPOT_SHELL —
      # wrap the same cat+/usr/bin/time cmd the GUI drivers got, so
      # the test surface is bit-identical across all five terminals.
      wrapper=$(mktemp /tmp/marspot-wrapper-XXXXXX)
      printf '#!/bin/bash\n%s\n' "$cmd" > "$wrapper"
      chmod 0755 "$wrapper"
      bin/drivers/marspot.sh run-shell-mcli "$wrapper" || launch_rc=$?
      ;;
  esac

  if (( launch_rc != 0 )); then
    echo "==> [${t}] driver failed rc=$launch_rc — skipping" >&2
    continue
  fi
  LAUNCHED_TERMS+=("$t")

  echo "==> [${t}] wait marker" >&2
  saw_done=0
  for _ in $(seq 1 "$MARKER_TIMEOUT_S"); do
    if [[ -s "$marker" ]] && grep -q '==ALL_DONE==' "$marker"; then
      saw_done=1
      break
    fi
    sleep 1
  done
  if (( saw_done == 0 )); then
    echo "==> [${t}] marker not done after ${MARKER_TIMEOUT_S}s — moving on" >&2
  fi

  echo "==> [${t}] close" >&2
  close_one "$t"
  sleep "$COOLDOWN_S"
done

# Note: window/process cleanup runs in the EXIT trap above so it
# triggers on success AND on any mid-run die (SIGINT from the
# LaunchAgent receiver, set -e from a driver, marker-wait timeout,
# anything). Don't duplicate cleanup here.

# Parse markers → JSON on stdout. Markers may be root-owned (when the
# surface ran under sudo asuser); they're mode 0644 world-readable so
# this user-mode parse can still read them.
python3 - "${SCENARIOS[@]}" <<'PY'
import json, re, os, sys, datetime, subprocess, platform
SCENARIOS = sys.argv[1:]
# Internal driver / marker names use "iterm" (matches iterm.sh); the
# baseline.json snapshot uses "iterm2" (matches the product / bundle id
# `com.googlecode.iterm2`). Map on the way out so the merge in the
# parent script doesn't create two separate competitor entries.
TERMS = ["iterm", "warp", "ghostty", "terminal", "marspot"]
TERM_TO_KEY = {"iterm": "iterm2"}

# App bundle paths per terminal — used to probe version + bundle_id so
# each refresh records what was measured against what build. Terminal.app
# lives under /System/Applications/Utilities on macOS 13+. marspot has
# no .app bundle; its version is probed separately via the binary.
APP_PATH = {
    "iterm":    "/Applications/iTerm.app",
    "warp":     "/Applications/Warp.app",
    "ghostty":  "/Applications/Ghostty.app",
    "terminal": "/System/Applications/Utilities/Terminal.app",
}

def plist_read(plist, key):
    try:
        return subprocess.check_output(
            ["defaults", "read", plist, key],
            text=True, stderr=subprocess.DEVNULL
        ).strip()
    except Exception:
        return None

def probe_version(term):
    if term == "marspot":
        # Source of truth for marspot version: git describe at build time
        # (build.rs sets MARSPOT_GIT_SHA). Try the binary itself first.
        bin_path = f"{ROOT}/target/release/marspot"
        if os.path.exists(bin_path):
            try:
                out = subprocess.check_output(
                    [bin_path, "--version"], text=True,
                    stderr=subprocess.DEVNULL, timeout=3
                ).strip()
                if out:
                    return out, "com.goliajp.marspot"
            except Exception:
                pass
        # Fallback: cargo metadata package.version + git short sha.
        try:
            cargo = subprocess.check_output(
                ["cargo", "metadata", "--no-deps", "--format-version=1"],
                cwd=ROOT, text=True, stderr=subprocess.DEVNULL, timeout=5
            )
            import json as _json
            data = _json.loads(cargo)
            for pkg in data.get("packages", []):
                if pkg.get("name") == "marspot":
                    v = pkg.get("version", "?")
                    sha = subprocess.check_output(
                        ["git", "-C", ROOT, "rev-parse", "--short", "HEAD"],
                        text=True, stderr=subprocess.DEVNULL, timeout=2
                    ).strip()
                    return f"{v} (git {sha})", "com.goliajp.marspot"
        except Exception:
            pass
        return None, "com.goliajp.marspot"
    p = APP_PATH.get(term)
    if not p: return None, None
    plist = f"{p}/Contents/Info.plist"
    if not os.path.exists(plist): return None, None
    v = plist_read(plist, "CFBundleShortVersionString") or ""
    build = plist_read(plist, "CFBundleVersion") or ""
    bundle = plist_read(plist, "CFBundleIdentifier") or ""
    version = f"{v} (build {build})" if build and build != v else v
    return version or None, bundle or None

def probe_host():
    out = {"name": platform.node().split(".")[0],
           "arch": f"{platform.machine()} ({platform.system()} {platform.release()})"}
    try:
        sw = subprocess.check_output(["sw_vers"], text=True)
        prod, ver, build = "macOS", "?", "?"
        for line in sw.splitlines():
            if line.startswith("ProductVersion:"): ver = line.split(":", 1)[1].strip()
            if line.startswith("BuildVersion:"):   build = line.split(":", 1)[1].strip()
        out["os"] = f"{prod} {ver} (build {build})"
    except Exception:
        out["os"] = "unknown"
    return out

ROOT = os.environ.get("PWD", os.getcwd())
today = datetime.date.today().isoformat()
out = {"host": probe_host()}
for t in TERMS:
    marker = f"/tmp/measure-{t}-all.txt"
    if not os.path.exists(marker):
        continue
    by_scn = {s: [] for s in SCENARIOS}
    current = None
    for line in open(marker, errors="replace"):
        m = re.match(r"^==SCN== (\S+)", line)
        if m:
            current = m.group(1)
            continue
        m = re.match(r"^real\s+(.+)$", line.strip())
        if m and current and current in by_scn:
            s = m.group(1)
            if "m" in s:
                a, b = s.split("m", 1)
                secs = int(a) * 60 + float(b.rstrip("s"))
            else:
                secs = float(s)
            by_scn[current].append(int(secs * 1e9))
    rec = {}
    version, bundle = probe_version(t)
    if version: rec["version"] = version
    if bundle:  rec["bundle_id"] = bundle
    rec["captured_at"] = today
    rec["method"] = ("ssh→LaunchAgent (com.marspot.bench-trigger)"
                     if os.environ.get("XPC_SERVICE_NAME", "").startswith("com.marspot")
                     else "console / Screen Sharing")
    for s, vs in by_scn.items():
        if not vs:
            continue
        vs.sort()
        med = vs[len(vs) // 2]
        size = os.path.getsize(f"{ROOT}/bench/scenarios/{s}.bin")
        bps = size * 1_000_000_000 // med
        rec[f"{s}_MBps"] = round(bps / 1048576, 1)
    # Only emit if we got at least one MBps measurement; metadata alone
    # isn't worth surfacing.
    if any(k.endswith("_MBps") for k in rec):
        out[TERM_TO_KEY.get(t, t)] = rec
print(json.dumps(out, indent=2))
PY
