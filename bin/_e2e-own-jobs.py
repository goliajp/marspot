# bin/_e2e-own-jobs.py [--self PID] — on a runner: does another job of
# marspot hold this host's bench lock?  A window suite does not share the
# host with one: a gate, a test run, a build, a measurement or another
# window run of ours.  Jobs of other projects are not looked at here (the
# in-lock check reads a window shared with them as such).
#
# Ours: the holder's command or note in `bench-lock status` names a run's
# scratch prefix (/tmp/msp-e2e-), a remote copy under ~/work whose name
# has marspot in it, or a window run's note ("marspot e2e"); or the lock
# log's take of that command, at the time status says it was taken,
# was asked from such a copy (the directory column).  PID is the holder
# asking, left out.
#
# Prints one `ours:` line per such holder (and a `note:` when the lock
# log could not be read); exit 0 none, 1 some, 2 the status could not be
# read.  E2E_BENCH_LOCK and E2E_LOCK_LOG name stand-ins for the tests.
import os, re, subprocess, sys, time

LOCK = os.environ.get("E2E_BENCH_LOCK", "/usr/local/bin/bench-lock")
LOG = os.environ.get("E2E_LOCK_LOG", "/Users/Shared/bench-lock.log")
NAMED = re.compile(r"/tmp/msp-e2e-|/work/[^/\s'\"]*marspot|marspot e2e ", re.I)
COPY = re.compile(r"/work/[^/]*marspot", re.I)
HOLDING = "holding since "
# the log's take is written a moment before status's entry
SKEW_S = 5
TAIL_BYTES = 1 << 20


def holders(text, me):
    """(pid, mode, holding field, command) of each holder but me; a command
    spanning lines is read from its first"""
    for line in text.splitlines():
        f = line.split("\t", 3)
        if len(f) == 4 and f[0].isdigit() and f[0] != me and f[2].startswith(HOLDING):
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
        print("note: lock log %s unread (%s): jobs of ours are told by their command only" % (LOG, e.strerror))
    found = 0
    for h in holders(st.stdout, me):
        named = NAMED.search(h[2] + "\t" + h[3])
        d = None if named or log is None else asked_from(h, log)
        if not named and not (d and COPY.search(d)):
            continue
        found += 1
        print("ours: pid=%s %s, %s: %s%s" % (h[0], h[1], h[2], h[3][:160], " (asked from %s)" % d if d else ""))
    return 1 if found else 0


if __name__ == "__main__":
    sys.dont_write_bytecode = True
    sys.exit(main(sys.argv[1:]))
