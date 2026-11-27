# bin/_e2e-own-jobs.py [--self PID] — on a runner: does another job of
# marspot hold this host's bench lock?  A window suite does not share the
# host with one: a gate, a test run, a build, a measurement or another
# window run of ours.  Jobs of other projects are not looked at here (the
# in-lock check reads a window shared with them as such).
#
# Ours: the holder's line in `bench-lock status` carries the note every
# script of ours gives the lock ("marspot-job KIND", bin/_lock-mark.sh),
# or names a run's scratch (/tmp/msp-e2e-, /tmp/marspot-) or a remote
# copy under ~/work whose name has marspot in it; or the lock log's take
# of that command, at the time status says it was taken, was asked from
# such a copy (the directory column).  PID is the holder asking, left
# out.  So is another window suite's hold (its command runs this check
# first) that took the lock after PID, or in the same second with a
# higher pid: its own check finds PID still holding and yields, so of
# two such holds the earlier runs and the later waits.  With the log
# unread, a holder whose line says none of that cannot be told from
# another project's: it is reported as unread, and the suite waits for
# it as for one of ours.
#
# Prints one `ours:` line per such holder and one `unread:` line per
# holder that cannot be told; exit 0 none of either, 1 some of ours, 2
# the status could not be read or a holder could not be told.
# E2E_BENCH_LOCK and E2E_LOCK_LOG name stand-ins for the tests.
import os, re, subprocess, sys, time

LOCK = os.environ.get("E2E_BENCH_LOCK", "/usr/local/bin/bench-lock")
LOG = os.environ.get("E2E_LOCK_LOG", "/Users/Shared/bench-lock.log")
NAMED = re.compile(r"marspot-job [a-z]|marspot e2e |/tmp/msp-e2e-|/tmp/marspot-|/work/[^/\s'\"]*marspot", re.I)
COPY = re.compile(r"/work/[^/]*marspot", re.I)
HOLDING = "holding since "
# the log's take is written a moment before status's entry
SKEW_S = 5
TAIL_BYTES = 1 << 20


# in the command of every window suite's hold
CHECKING = "bin/_e2e-own-jobs.py --self "


def holders(text):
    """(pid, mode, holding field, command) of each holder; a command
    spanning lines is read from its first"""
    for line in text.splitlines():
        f = line.split("\t", 3)
        if len(f) == 4 and f[0].isdigit() and f[2].startswith(HOLDING):
            cmd = f[3][: -len(" [stopped]")] if f[3].endswith(" [stopped]") else f[3]
            yield f[0], f[1], f[2], cmd


def takes(text):
    """(epoch, command, directory) of each take with a directory column"""
    for line in text.splitlines():
        f = line.split("\t")
        if len(f) >= 6 and f[0].isdigit() and f[3] == "took":
            yield int(f[0]), f[4], f[5]


def since(field):
    return time.mktime(time.strptime(field[len(HOLDING):][:19], "%Y-%m-%d %H:%M:%S"))


def order(h):
    """when a holder took the lock, then its pid: the earlier of two goes first"""
    try:
        return since(h[2]), int(h[0])
    except ValueError:
        return None


def yields_to(h, mine):
    """h is another window suite's hold, taken after mine"""
    if mine is None or CHECKING not in h[3]:
        return False
    theirs = order(h)
    return theirs is not None and theirs > mine


def asked_from(h, log):
    """the directory the lock log says this holder's take was asked from, or None"""
    cmd = h[3].replace("\t", " ")[:200]
    try:
        at = since(h[2])
    except ValueError:
        return None
    for t, c, d in takes(log):
        if abs(t - at) <= SKEW_S and c.startswith(cmd):
            return d
    return None


def tail(path):
    with open(path, "rb") as f:
        f.seek(0, os.SEEK_END)
        f.seek(max(0, f.tell() - TAIL_BYTES))
        return f.read().decode(errors="replace")


def main(argv):
    me = argv[argv.index("--self") + 1] if "--self" in argv else ""
    try:
        st = subprocess.run([LOCK, "status"], capture_output=True, text=True, timeout=20)
    except (OSError, subprocess.TimeoutExpired) as e:
        print("unread: bench-lock status: %s" % e)
        return 2
    if st.returncode != 0:
        print("unread: bench-lock status exited %d" % st.returncode)
        return 2
    try:
        log = tail(LOG)
    except OSError as e:
        log = None
        print("note: lock log %s unread (%s): holders are told by their line in status only" % (LOG, e.strerror))
    found = unknown = 0
    all_holders = list(holders(st.stdout))
    mine = next((order(h) for h in all_holders if h[0] == me), None)
    for h in all_holders:
        if h[0] == me or yields_to(h, mine):
            continue
        named = NAMED.search(h[2] + "\t" + h[3])
        if not named and log is None:
            unknown += 1
            print("unread: pid=%s %s, %s: %s (ours or not cannot be told without the lock log)"
                  % (h[0], h[1], h[2], h[3][:160]))
            continue
        d = None if named else asked_from(h, log)
        if not named and not (d and COPY.search(d)):
            continue
        found += 1
        print("ours: pid=%s %s, %s: %s%s" % (h[0], h[1], h[2], h[3][:160], " (asked from %s)" % d if d else ""))
    if found:
        return 1
    return 2 if unknown else 0


if __name__ == "__main__":
    sys.dont_write_bytecode = True
    sys.exit(main(sys.argv[1:]))
