#!/usr/bin/env python3
"""A/B two marspot builds through the LIVE pipeline, honestly.

Why this exists as a tool rather than a one-off: every live measurement
in this repo has to fight the same three things, and each of them is
easy to get wrong in a way that produces a confident wrong number.

  1. `time cat` measures a terminal only while `cat` is BLOCKED on PTY
     writes.  Under that, the kernel buffer eats the file and the run
     reports nothing at all — a 64 KiB scenario times 0 ms on every
     terminal ever tested here.  The faster the terminal, the longer it
     keeps `cat` unblocked, so small samples flatter slow terminals and
     libel fast ones.  Hence the size ladder: read throughput off the
     BIG end, and use the small end only to see how much of the window
     is warm-up.

  2. The bench host is shared.  The same commit measured 130 ms and
     290 ms an hour apart because another project was compiling; a
     `git bisect` run on those numbers happily blamed a 13-line
     keyboard-mapping commit.  Hence interleaving (A,B,A,B,… so drift
     hits both arms) and min-of-N (load can only ADD time, so the
     minimum is the least-contaminated sample, where a median still
     carries the contamination).

  3. A live trial spawns a real terminal that writes real scrollback.
     Pointed at the default state dir, that is the state dir of the
     terminal the user is sitting in.  Every trial here gets a fresh
     temporary one.

Usage:
    bin/ab-live.py A=/path/to/mcli B=/path/to/other-mcli
    bin/ab-live.py A=... B=... --scenarios cat-emoji,cat-cjk
    bin/ab-live.py A=... B=... --sizes 1,4,16 --rounds 5
    bin/ab-live.py A=... B=... --env B:MARSPOT_DISK_SCROLLBACK=0

`--sizes` are multiples of the scenario file, cat'ed as one command.
Output is one row per (arm, scenario, size) with the min and the
implied MB/s, plus the B-vs-A delta at the largest size.
"""
import argparse, json, os, shutil, subprocess, sys, tempfile, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCEN = os.path.join(ROOT, "bench", "scenarios")


def run_trial(binary, env_extra, paths, timeout=600):
    """One `time cat <paths...>` inside a fresh terminal.  ms, or None."""
    state = tempfile.mkdtemp(prefix="marspot-ablive-")
    marker = os.path.join(state, "marker.txt")
    cmd = os.path.join(state, "cmd.sh")
    with open(cmd, "w") as f:
        f.write("#!/bin/sh\n/usr/bin/time -p /bin/cat %s 2> %s\n" % (" ".join(paths), marker))
    os.chmod(cmd, 0o755)
    env = dict(os.environ, MARSPOT_SHELL=cmd, MARSPOT_STATE_DIR=state, **env_extra)
    p = subprocess.Popen([binary], env=env, stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL)
    deadline = time.time() + timeout
    try:
        while time.time() < deadline:
            if os.path.exists(marker) and os.path.getsize(marker) > 0:
                break
            time.sleep(0.05)
        p.terminate()
        try:
            p.wait(5)
        except subprocess.TimeoutExpired:
            p.kill()
        if not os.path.exists(marker):
            return None
        for line in open(marker).read().splitlines():
            if line.startswith("real"):
                return float(line.split()[1]) * 1000.0
        return None
    finally:
        shutil.rmtree(state, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("arms", nargs="+", help="NAME=/path/to/mcli (two or more)")
    ap.add_argument("--scenarios", default="cat-emoji")
    ap.add_argument("--sizes", default="1,4", help="multiples of the scenario file")
    ap.add_argument("--rounds", type=int, default=5)
    ap.add_argument("--env", action="append", default=[],
                    help="ARM:KEY=VALUE extra environment for one arm")
    ap.add_argument("--json", help="also write raw samples here")
    a = ap.parse_args()

    arms = []
    for spec in a.arms:
        name, _, path = spec.partition("=")
        if not path or not os.path.exists(path):
            sys.exit(f"arm {name!r}: no binary at {path!r}")
        arms.append((name, path))
    extra = {name: {} for name, _ in arms}
    for e in a.env:
        arm, _, kv = e.partition(":")
        k, _, v = kv.partition("=")
        extra.setdefault(arm, {})[k] = v

    scenarios = [s for s in a.scenarios.split(",") if s]
    sizes = [int(x) for x in a.sizes.split(",") if x]
    for s in scenarios:
        if not os.path.exists(os.path.join(SCEN, s + ".bin")):
            sys.exit(f"missing {s}.bin — run bin/gen-scenarios.sh")

    samples = {}
    for rnd in range(a.rounds):
        for name, path in arms:                     # interleaved: drift hits both arms
            for s in scenarios:
                f = os.path.join(SCEN, s + ".bin")
                for n in sizes:
                    ms = run_trial(path, extra.get(name, {}), [f] * n)
                    if ms is not None:
                        samples.setdefault((name, s, n), []).append(ms)
        print(f"  round {rnd + 1}/{a.rounds} done", file=sys.stderr)

    print(f"\n{'scenario':11s} {'MB':>7s} " +
          "  ".join(f"{name:>18s}" for name, _ in arms))
    for s in scenarios:
        size = os.path.getsize(os.path.join(SCEN, s + ".bin"))
        for n in sizes:
            mb = size * n / 1e6
            cells = []
            for name, _ in arms:
                v = samples.get((name, s, n))
                cells.append(f"{min(v):7.0f}ms {mb / (min(v) / 1000):6.1f}MB/s" if v else f"{'—':>18s}")
            print(f"{s:11s} {mb:7.1f} " + "  ".join(cells))
        base = samples.get((arms[0][0], s, sizes[-1]))
        if base:
            for name, _ in arms[1:]:
                v = samples.get((name, s, sizes[-1]))
                if v:
                    print(f"{'':11s} {'':7s} {name} vs {arms[0][0]} at {sizes[-1]}x: "
                          f"{(min(base) - min(v)) / min(base) * 100:+.1f}%")
    if a.json:
        json.dump({f"{k[0]}|{k[1]}|{k[2]}": v for k, v in samples.items()},
                  open(a.json, "w"), indent=1)
        print(f"\nraw samples → {a.json}")


if __name__ == "__main__":
    main()
