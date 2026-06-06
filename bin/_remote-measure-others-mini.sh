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
TERMS=(iterm warp ghostty terminal)
MARKER_TIMEOUT_S=600

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

# macOS ships bash 3.2 → no `declare -A`. Use one var per terminal.
WIN_iterm=""
WIN_warp=""
WIN_ghostty=""
WIN_terminal=""
LAUNCHED_TERMS=()
for t in "${TERMS[@]}"; do
  # In ssh mode, only Ghostty is reachable (see header comment for
  # why iTerm/Warp/Terminal are unreachable from ssh). Skip the rest;
  # their previous baseline snapshot stays intact via per-terminal
  # merge in the parent script. To refresh iTerm/Warp/Terminal use the
  # LaunchAgent route (bin/install-bench-launchagent.sh) so the
  # measure runs inside the user's Aqua session, not over ssh.
  if [[ $SSH_MODE -eq 1 && "$t" != "ghostty" ]]; then
    echo "==> skip $t (ssh mode — use LaunchAgent trigger for full refresh)" >&2
    continue
  fi
  marker="/tmp/measure-${t}-all.txt"
  # Marker may be root-owned from a previous ssh-mode ghostty run; use
  # sudo to be safe. Local/LaunchAgent runs just succeed on plain rm.
  rm -f "$marker" 2>/dev/null || sudo -n rm -f "$marker" 2>/dev/null || true
  cmd="$(build_one_cmd "$marker")"
  echo "==> launch $t" >&2
  # Driver failures are tolerated — under LaunchAgent SE keystroke is
  # TCC-blocked for /usr/bin/osascript (system TCC.db is SIP write-
  # protected, can't be granted from ssh) so Warp specifically fails
  # rc=1 here. Other drivers may also throw; keep going so the run still
  # returns whatever subset DID work. Failed terminal's previous-day
  # snapshot stays in baseline via per-terminal merge.
  launch_rc=0
  case "$t" in
    iterm)    WIN_iterm="$(bin/drivers/iterm.sh run-tabs 1 "$cmd" 2>&1)" || launch_rc=$? ;;
    warp)     bin/drivers/warp.sh run-single "$cmd" || launch_rc=$? ;;
    # Ghostty's binary needs full Aqua launchd domain (NSApp init +
    # Metal) → ASUSER (root) under ssh. Local runs use empty prefix.
    ghostty)  $ASUSER bin/drivers/ghostty.sh run-single "$cmd" || launch_rc=$? ;;
    terminal) WIN_terminal="$(bin/drivers/terminal.sh run-single "$cmd" 2>&1)" || launch_rc=$? ;;
  esac
  if (( launch_rc != 0 )); then
    echo "==> $t driver failed rc=$launch_rc — continuing without it" >&2
    continue
  fi
  LAUNCHED_TERMS+=("$t")
done

for t in "${LAUNCHED_TERMS[@]}"; do
  marker="/tmp/measure-${t}-all.txt"
  echo "==> wait $t marker" >&2
  for _ in $(seq 1 "$MARKER_TIMEOUT_S"); do
    [[ -s "$marker" ]] && grep -q '==ALL_DONE==' "$marker" && break
    sleep 1
  done
done

# Close windows by id (skip warp + ghostty — their drivers explicitly
# refuse to quit the app to avoid killing the user's session; their
# marker shells already exited via the trailing `exit`).
[[ -n "$WIN_iterm" ]]    && bin/drivers/iterm.sh    close-windows "$WIN_iterm"    >/dev/null 2>&1 || true
[[ -n "$WIN_terminal" ]] && bin/drivers/terminal.sh close-windows "$WIN_terminal" >/dev/null 2>&1 || true

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
TERMS = ["iterm", "warp", "ghostty", "terminal"]
TERM_TO_KEY = {"iterm": "iterm2"}

# App bundle paths per terminal — used to probe version + bundle_id so
# each refresh records what was measured against what build. Terminal.app
# lives under /System/Applications/Utilities on macOS 13+.
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
