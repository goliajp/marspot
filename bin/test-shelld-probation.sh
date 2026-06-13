#!/usr/bin/env bash
#
# shelld update probation + rollback test.  shelld is the most
# dangerous update layer (its restart SIGHUPs every session), gated
# behind explicit `install-shelld.sh --apply-pending`.  After promoting
# pending → current and re-bootstrapping the daemon, the script watches
# a 30 s probation window and, if the new daemon isn't running at the
# END of it, quarantines the bad binary, restores prev/ into current/
# (+ bundle), and re-bootstraps — logging SHELLD_PROBATION_FAIL /
# SHELLD_ROLLBACK.  That recovery path had no automated test.
#
# SAFETY: this drives the REAL install-shelld.sh logic but against a
# throwaway, pid-suffixed LaunchAgent label + an isolated state dir +
# fake `sh -c` daemons — it NEVER touches the installed
# `com.marspot.shelld` daemon, its plist, or the user's binary tree.
# The env overrides (MARSPOT_SHELLD_{LABEL,BIN,PLIST}, MARSPOT_STATE_DIR)
# default to production values when unset, so the production install
# path is byte-for-byte unchanged.
#
# Each case uses a SHORT probation (MARSPOT_SHELLD_PROBATION_S=10).
# Budget ~40 s.  Run after `cargo build --release` (only needs the
# script + launchctl + /bin/sh — no marspot binaries).

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UID_NUM="$(id -u)"
INSTALL="$ROOT/bin/install-shelld.sh"

# Unique, pid-suffixed so it can never collide with the real daemon or
# a parallel run.
LABEL="com.marspot.shelld.probtest.$$"
STATE_DIR="/tmp/marspot-shelld-probtest.$$"
PLIST="$STATE_DIR/test.plist"
BUNDLE="$STATE_DIR/fake-bundle/marspot-shelld"
TREE="$STATE_DIR/binaries"
SUP_LOG="$STATE_DIR/logs/supervisor.log"

export MARSPOT_STATE_DIR="$STATE_DIR"
export MARSPOT_SHELLD_LABEL="$LABEL"
export MARSPOT_SHELLD_PLIST="$PLIST"
export MARSPOT_SHELLD_BIN="$BUNDLE"
export MARSPOT_SHELLD_PROBATION_S=10

fail() {
  echo "FAIL: $*"
  echo "  (supervisor log):"
  tail -15 "$SUP_LOG" 2>/dev/null | sed 's/^/    /'
  echo "  (launchctl):"
  launchctl print "gui/$UID_NUM/$LABEL" 2>&1 | grep -E 'state|path' | head -5 | sed 's/^/    /'
  exit 1
}

cleanup() {
  launchctl bootout "gui/$UID_NUM/$LABEL" >/dev/null 2>&1 || true
  rm -rf "$STATE_DIR"
}
trap cleanup EXIT

# Guard: refuse to run if the resolved label is somehow the real one.
[[ "$LABEL" == com.marspot.shelld.probtest.* ]] \
  || fail "refusing to run — label '$LABEL' is not a throwaway test label"

mk_daemon() {  # $1=path $2=good|bad
  mkdir -p "$(dirname "$1")"
  if [[ "$2" == good ]]; then
    printf '#!/bin/sh\nexec sleep 100000\n' > "$1"
  else
    printf '#!/bin/sh\nexit 1\n' > "$1"
  fi
  chmod +x "$1"
}

is_running() {
  launchctl print "gui/$UID_NUM/$LABEL" 2>&1 | grep -q "state = running"
}

# Fresh install with a GOOD daemon so the plist exists + daemon runs,
# the precondition --apply-pending assumes.
bootstrap_good_install() {
  launchctl bootout "gui/$UID_NUM/$LABEL" >/dev/null 2>&1 || true
  rm -rf "$STATE_DIR"
  mk_daemon "$BUNDLE" good
  "$INSTALL" >/tmp/marspot-probtest-install.out 2>&1 \
    || fail "initial install-shelld did not come up"
  is_running || fail "initial daemon not running after install"
}

# --- Case 1: GOOD pending survives probation → STABLE ----------------
echo "--- case: good pending survives probation ---"
bootstrap_good_install
mkdir -p "$TREE/pending"
mk_daemon "$TREE/pending/marspot-shelld" good
"$INSTALL" --apply-pending --yes >/tmp/marspot-probtest-good.out 2>&1
rc=$?
[[ $rc -eq 0 ]] || fail "[good] --apply-pending exited $rc (expected 0)"
grep -q "SHELLD_UPDATE_STABLE" "$SUP_LOG" 2>/dev/null \
  || fail "[good] no SHELLD_UPDATE_STABLE logged"
is_running || fail "[good] daemon not running after stable update"
[[ ! -e "$TREE/pending/marspot-shelld" ]] || fail "[good] pending/ not consumed"
echo "    OK — survived probation, STABLE, pending consumed"

# --- Case 2: BAD pending fails probation → auto rollback to prev/ -----
echo "--- case: bad pending fails probation, rolls back ---"
bootstrap_good_install
# Seed current/ with a known-GOOD binary; promote moves it to prev/,
# giving the rollback something to restore.
mkdir -p "$TREE/current" "$TREE/pending"
printf '#!/bin/sh\nexec sleep 100000\n# GOOD-OLD\n' > "$TREE/current/marspot-shelld"
chmod +x "$TREE/current/marspot-shelld"
mk_daemon "$TREE/pending/marspot-shelld" bad
"$INSTALL" --apply-pending --yes >/tmp/marspot-probtest-bad.out 2>&1
rc=$?
[[ $rc -eq 1 ]] || fail "[bad] --apply-pending exited $rc (expected 1 on rollback)"
grep -q "SHELLD_PROBATION_FAIL" "$SUP_LOG" 2>/dev/null \
  || fail "[bad] no SHELLD_PROBATION_FAIL logged"
grep -q "SHELLD_ROLLBACK.*re-bootstrapped and running" "$SUP_LOG" 2>/dev/null \
  || fail "[bad] no successful SHELLD_ROLLBACK logged"
# The bad binary was quarantined; current/ holds the restored GOOD-OLD.
grep -q "GOOD-OLD" "$TREE/current/marspot-shelld" 2>/dev/null \
  || fail "[bad] current/ is not the restored prev/ (GOOD-OLD) binary"
grep -q "exit 1" "$TREE/quarantine/marspot-shelld" 2>/dev/null \
  || fail "[bad] bad binary was not quarantined"
is_running || fail "[bad] daemon not running again after rollback"
echo "    OK — probation failed, rolled back to prev/, daemon running"

cleanup
trap - EXIT
echo "ALL PASS — shelld probation stabilises good updates and auto-rolls-back bad ones"
