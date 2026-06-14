#!/usr/bin/env bash
# bin/install-shelld.sh — install marspot-shelld as a LaunchAgent.
#
# Writes ~/Library/LaunchAgents/com.marspot.shelld.plist that points
# at the binary inside ~/.local/Marspot.app and bootstraps it via
# launchctl so it starts now and on every login.
#
# Usage:
#   bin/install-shelld.sh                       # install + start
#   bin/install-shelld.sh --uninstall           # stop + remove plist
#   bin/install-shelld.sh --status              # print runtime status
#   bin/install-shelld.sh --apply-pending       # silently-update shelld
#                                               # from binaries/pending/
#                                               # WARNING: kills sessions
#   bin/install-shelld.sh --apply-pending-execv # in-place silent update
#                                               # via SIGUSR1; PRESERVES
#                                               # every open session
#
# Idempotent: re-running is a no-op apart from refreshing the plist
# content (so a binary path change propagates next time).

set -euo pipefail

# Production defaults.  Every value is overridable so a sandbox test
# can drive the real promote/probation/rollback logic against a
# throwaway LaunchAgent without ever touching the installed daemon.
# With none of these env vars set, the resolved paths are byte-for-byte
# what they always were — the production install path is unchanged.
#
#   MARSPOT_SHELLD_LABEL  LaunchAgent label (default com.marspot.shelld)
#   MARSPOT_SHELLD_BIN    bundle binary the plist points at
#   MARSPOT_SHELLD_PLIST  plist path
#   MARSPOT_STATE_DIR     state root — when set, BIN_TREE / SUP_LOG /
#                         daemon logs all move into the sandbox,
#                         mirroring marspot::paths.
LABEL="${MARSPOT_SHELLD_LABEL:-com.marspot.shelld}"
PLIST="${MARSPOT_SHELLD_PLIST:-$HOME/Library/LaunchAgents/${LABEL}.plist}"
BIN="${MARSPOT_SHELLD_BIN:-$HOME/.local/Marspot.app/Contents/MacOS/marspot-shelld}"
# State-dir-aware roots, matching marspot::paths {state_root,log_dir}:
# a set MARSPOT_STATE_DIR keeps everything in the sandbox so one
# `rm -rf` cleans up; unset → the installed app's conventional dirs.
if [[ -n "${MARSPOT_STATE_DIR:-}" ]]; then
  BIN_TREE="$MARSPOT_STATE_DIR/binaries"
  SUP_LOG="$MARSPOT_STATE_DIR/logs/supervisor.log"
  LOG_DIR="$MARSPOT_STATE_DIR/logs"
else
  BIN_TREE="$HOME/Library/Caches/marspot/binaries"
  SUP_LOG="$HOME/Library/Logs/Marspot/supervisor.log"
  LOG_DIR="$HOME/Library/Logs/marspot"
fi
LOG_OUT="$LOG_DIR/shelld.log"
LOG_ERR="$LOG_DIR/shelld.err"

# Append one event to the structured marspot.log stream so daemon-
# install events appear on the same TSV timeline as core / shell /
# shelld events. Routes through `marspot-shelld --log-event` so the
# event picks up component=shelld, pid, tid, ms timestamps, AND the
# rotation + GC pipeline — bash can't drift away from the Rust format.
#
# Fall-back: if no marspot-shelld binary is reachable yet (very first
# install before any binary exists), write the legacy supervisor.log
# directly so we don't lose the early-install event.
sup_log() {
  local tag="$1"; shift
  local detail="$*"
  # New stream — structured marspot.log via shelld CLI. Best-effort.
  if [[ -x "$BIN" ]]; then
    "$BIN" --log-event "$tag" "$detail" >/dev/null 2>&1 || true
  elif [[ -x "$BIN_TREE/current/marspot-shelld" ]]; then
    "$BIN_TREE/current/marspot-shelld" --log-event "$tag" "$detail" >/dev/null 2>&1 || true
  fi
  # Legacy stream — supervisor.log TSV, until every soak/integration
  # script grepping it has been migrated to marspot.log. Best-effort.
  mkdir -p "$(dirname "$SUP_LOG")" 2>/dev/null
  printf '%s\t%s\t%s\n' "$(date +%s.%N)" "$tag" "$detail" >> "$SUP_LOG" 2>/dev/null
}

# Reload the LaunchAgent so a swapped binary / changed plist takes effect.
# `launchctl bootout` is ASYNC: a fixed `sleep` after it races the next
# `bootstrap`, which then fails ("service already loaded" / I/O error).
# Under `set -e` that aborted the script with shelld booted-OUT and dead —
# and KeepAlive can't resurrect an UNregistered agent, so the daemon
# (and every session) stayed down until a manual re-bootstrap. (This is
# exactly how a `--with-shelld` update bricked the live app once.)
# Fix: bootout, POLL until the service is truly gone, then bootstrap with
# retries. Never returns leaving the agent booted-out.
reload_agent() {
  local domain="gui/$(id -u)" svc="gui/$(id -u)/$LABEL" i
  if launchctl print "$svc" >/dev/null 2>&1; then
    launchctl bootout "$svc" 2>/dev/null || true
    for i in $(seq 1 50); do                      # up to ~5s for it to vanish
      launchctl print "$svc" >/dev/null 2>&1 || break
      sleep 0.1
    done
  fi
  for i in $(seq 1 30); do                        # retry bootstrap ~6s
    if launchctl bootstrap "$domain" "$PLIST" 2>/dev/null; then
      return 0
    fi
    sleep 0.2
  done
  echo "error: launchctl bootstrap $LABEL failed after retries:" >&2
  launchctl bootstrap "$domain" "$PLIST" 2>&1 | head -3 >&2
  return 1
}

case "${1:-}" in
  --uninstall)
    if launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
      launchctl bootout "gui/$(id -u)/$LABEL" 2>&1 || true
    fi
    rm -f "$PLIST"
    echo "uninstalled."
    exit 0
    ;;
  --status)
    echo "plist: $PLIST"
    [[ -f "$PLIST" ]] && echo "  exists" || echo "  MISSING"
    echo "launchctl:"
    launchctl print "gui/$(id -u)/$LABEL" 2>&1 | head -20 || true
    echo "socket:"
    ls -la "$HOME/Library/Caches/marspot/shelld.sock" 2>&1 || true
    echo "pending shelld:"
    if [[ -f "$BIN_TREE/pending/marspot-shelld" ]]; then
      ls -la "$BIN_TREE/pending/marspot-shelld"
      echo "  (apply with: bin/install-shelld.sh --apply-pending)"
    else
      echo "  (none)"
    fi
    exit 0
    ;;
  --apply-pending)
    pending="$BIN_TREE/pending/marspot-shelld"
    if [[ ! -f "$pending" ]]; then
      echo "no $pending — nothing to apply"
      exit 1
    fi
    cat <<MSG
shelld update available at $pending.

Applying it will:
  - bootout the running daemon
  - move pending → current → bundle binary
  - bootstrap the new daemon

All currently-open shelld sessions WILL DIE (every shell child gets
SIGHUP when shelld's PTY parent fds close).  Save anything you care
about first.

MSG
    if [[ "${2:-}" != "--yes" ]]; then
      read -rp "Continue? (yes/no) " resp
      [[ "$resp" == "yes" ]] || { echo "aborted."; exit 1; }
    fi
    sup_log "SHELLD_UPDATE_APPLY" "promote + bundle install"

    # Promote pending → current.  Match BinaryTree::promote_pending.
    mkdir -p "$BIN_TREE/current" "$BIN_TREE/prev"
    [[ -f "$BIN_TREE/current/marspot-shelld" ]] && \
      mv -f "$BIN_TREE/current/marspot-shelld" "$BIN_TREE/prev/marspot-shelld"
    mv "$pending" "$BIN_TREE/current/marspot-shelld"
    # Strip Gatekeeper xattrs so the new daemon doesn't stall in
    # _dyld_start.
    xattr -d com.apple.quarantine  "$BIN_TREE/current/marspot-shelld" 2>/dev/null || true
    xattr -d com.apple.provenance  "$BIN_TREE/current/marspot-shelld" 2>/dev/null || true

    # Copy into the bundle so the LaunchAgent plist (which points at
    # the bundle path) picks it up on next bootstrap.
    cp "$BIN_TREE/current/marspot-shelld" "$BIN"

    # Restart the daemon.
    reload_agent

    # Probation: poll every 5 s for 30 s.  KeepAlive +
    # ThrottleInterval=5 make a crashing daemon flap through
    # "running", so a single early check proves nothing — only
    # still-running at the END of the window counts as stable.
    PROBATION_S="${MARSPOT_SHELLD_PROBATION_S:-30}"
    POLL_S=5
    elapsed=0
    running=no
    while (( elapsed < PROBATION_S )); do
      sleep "$POLL_S"; elapsed=$(( elapsed + POLL_S ))
      if launchctl print "gui/$(id -u)/$LABEL" 2>&1 | grep -q "state = running"; then
        running=yes
      else
        running=no
      fi
      echo "  probation ${elapsed}/${PROBATION_S}s — state: $running"
    done
    if [[ "$running" == yes ]]; then
      sup_log "SHELLD_UPDATE_STABLE" "daemon survived ${PROBATION_S}s probation"
      echo "shelld updated and running."
      exit 0
    fi

    # Probation failed — quarantine the broken binary and restore
    # prev/ into current/ + the bundle path, then re-bootstrap.
    sup_log "SHELLD_PROBATION_FAIL" "daemon not running after ${PROBATION_S}s"
    echo "shelld didn't survive probation — rolling back." >&2
    if [[ ! -f "$BIN_TREE/prev/marspot-shelld" ]]; then
      sup_log "SHELLD_ROLLBACK" "no prev/ to restore — manual recovery required"
      echo "error: no $BIN_TREE/prev/marspot-shelld to roll back to." >&2
      echo "       check $LOG_ERR, then reinstall a known-good shelld." >&2
      exit 1
    fi
    mkdir -p "$BIN_TREE/quarantine"
    mv -f "$BIN_TREE/current/marspot-shelld" "$BIN_TREE/quarantine/marspot-shelld"
    mv "$BIN_TREE/prev/marspot-shelld" "$BIN_TREE/current/marspot-shelld"
    cp "$BIN_TREE/current/marspot-shelld" "$BIN"
    reload_agent
    sleep 1
    if launchctl print "gui/$(id -u)/$LABEL" 2>&1 | grep -q "state = running"; then
      sup_log "SHELLD_ROLLBACK" "prev/ restored, daemon re-bootstrapped and running"
      echo "rolled back to previous shelld — daemon running again." >&2
    else
      sup_log "SHELLD_ROLLBACK" "prev/ restored but daemon still not running"
      echo "rolled back, but daemon still not running — check $LOG_ERR" >&2
    fi
    exit 1
    ;;
  --apply-pending-execv)
    # In-place silent update: SIGUSR1 the running daemon, which does
    # its own promote_pending() + execv self-update preserving the
    # listen socket + every PTY master fd. Sessions survive across the
    # swap; clients drop and reattach via bytelog replay. Use this on
    # a healthy running shelld; if shelld isn't running, fall back to
    # --apply-pending (the bootout/bootstrap path).
    pending="$BIN_TREE/pending/marspot-shelld"
    if [[ ! -f "$pending" ]]; then
      echo "no $pending — nothing to apply"
      exit 1
    fi

    pid="$(launchctl print "gui/$(id -u)/$LABEL" 2>/dev/null \
      | awk -F'=' '/^\tpid =/{gsub(/[ \t]/,"",$2); print $2; exit}')"
    if [[ -z "$pid" || "$pid" -le 0 ]]; then
      echo "shelld not running for $LABEL — use --apply-pending (cold-restart path)" >&2
      exit 1
    fi

    sup_log "SHELLD_UPDATE_APPLY_EXECV" "pid=$pid → SIGUSR1"

    # Update the bundle with the new bytes FIRST, so a future cold
    # restart (launchd KeepAlive after an unrelated death) picks up
    # the new image rather than reverting to whatever's still in the
    # bundle path. cp not mv: leave pending intact so shelld can
    # consume it via promote_pending() inside do_execv_swap.
    cp "$pending" "$BIN"
    xattr -d com.apple.quarantine "$BIN" 2>/dev/null || true
    xattr -d com.apple.provenance "$BIN" 2>/dev/null || true

    if ! kill -USR1 "$pid"; then
      sup_log "SHELLD_PROBATION_FAIL" "kill -USR1 $pid failed"
      echo "kill -USR1 $pid failed" >&2
      exit 1
    fi

    # Probation: shelld's PID must NOT change (= execv preserved
    # identity) AND must stay alive throughout the window. PID change
    # implies launchd respawned us = the in-place swap died and the
    # old image's children were SIGHUP'd; that's the bootout-equivalent
    # failure mode. The execv-fail path inside shelld self-rolls back
    # and keeps running on the old image, so a same-pid survival means
    # either the swap succeeded or shelld declined gracefully.
    PROBATION_S="${MARSPOT_SHELLD_PROBATION_S:-30}"
    POLL_S=2
    elapsed=0
    while (( elapsed < PROBATION_S )); do
      sleep "$POLL_S"; elapsed=$(( elapsed + POLL_S ))
      cur_pid="$(launchctl print "gui/$(id -u)/$LABEL" 2>/dev/null \
        | awk -F'=' '/^\tpid =/{gsub(/[ \t]/,"",$2); print $2; exit}')"
      if [[ -z "$cur_pid" ]] || ! kill -0 "$pid" 2>/dev/null; then
        sup_log "SHELLD_PROBATION_FAIL" "execv path: shelld pid $pid not alive at ${elapsed}s"
        echo "shelld died after SIGUSR1 — sessions LOST" >&2
        echo "       check $LOG_ERR for the new image's startup logs" >&2
        exit 1
      fi
      if [[ "$cur_pid" != "$pid" ]]; then
        sup_log "SHELLD_PROBATION_FAIL" "execv path: pid changed $pid → $cur_pid (launchd respawn)"
        echo "shelld pid changed $pid → $cur_pid — execv failed, launchd respawned" >&2
        echo "       sessions LOST; check $LOG_ERR" >&2
        exit 1
      fi
    done
    sup_log "SHELLD_UPDATE_STABLE" "execv path: pid=$pid survived ${PROBATION_S}s probation"
    echo "shelld updated via execv — sessions preserved, pid $pid"
    exit 0
    ;;
  ""|--install)
    ;;
  *)
    echo "unknown arg: $1" >&2; exit 2 ;;
esac

if [[ ! -x "$BIN" ]]; then
  echo "error: binary not found at $BIN" >&2
  echo "       build with: cargo build --release --bin marspot-shelld" >&2
  echo "       and install into ~/.local/Marspot.app/Contents/MacOS/" >&2
  exit 1
fi

mkdir -p "$LOG_DIR"
mkdir -p "$(dirname "$PLIST")"

cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>${BIN}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>${LOG_OUT}</string>
  <key>StandardErrorPath</key>
  <string>${LOG_ERR}</string>
  <key>ThrottleInterval</key>
  <integer>5</integer>
  <key>ProcessType</key>
  <string>Interactive</string>
</dict>
</plist>
EOF

# Reload so a content change to the plist (or a swapped binary) takes
# effect — race-safe (see reload_agent: poll-until-gone + retry, never
# leaves shelld booted-out).
reload_agent

# Verify
sleep 1
if launchctl print "gui/$(id -u)/$LABEL" 2>&1 | grep -q "state = running"; then
  echo "shelld running, socket at $HOME/Library/Caches/marspot/shelld.sock"
else
  echo "warning: shelld doesn't show running state — check $LOG_ERR" >&2
  launchctl print "gui/$(id -u)/$LABEL" 2>&1 | head -30
  exit 1
fi
