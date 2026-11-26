#!/usr/bin/env python3
"""`_size_budget.py --self-test`: the size budget rules can fail and can pass.

Builds a throwaway root (Cargo.toml, bench/size-budget.json, the size
tables) from a fabricated link map, then checks the map summary, the
matching rule, and the bench gate itself on fabricated size readings:
under budget passes and leaves the budget files alone, over budget
fails, and a budget raised without its table is refused.
"""
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile

import _size_budget as sb
import _size_map

HERE = os.path.dirname(os.path.abspath(__file__))

MAP = """# Path: /x/marspot-shell
# Arch: arm64
# Object files:
[  0] linker synthesized
[  1] /x/lto.o
# Sections:
# Address\tSize    \tSegment\tSection
0x100000000\t0x00010000\t__TEXT\t__text
0x100010000\t0x00001000\t__TEXT\t__cstring
0x100020000\t0x00004000\t__DATA\t__bss
# Symbols:
# Address\tSize    \tFile  Name
0x100000000\t0x00003000\t[  1] __ZN12marspot_term6parser6Parser7advance17h0123456789abcdefE
0x100003000\t0x00001000\t[  1] __ZN83_$LT$marspot_term..grid..Grid$u20$as$u20$core..fmt..Debug$GT$3fmt17h0123456789abcdefE
0x100004000\t0x00002000\t[  1] __ZN4core3fmt5write17h0123456789abcdefE
0x100006000\t0x00000900\t[  1] __RNvNtCs1234_13marspot_shell7windows4open
0x100006900\t0x00000100\t[  1] _objc_msgSend$alloc
0x100007000\t0x00001000\t[  1] __ZN13marspot_shell7plugins10claudecode4scan17h0123456789abcdefE
0x100008000\t0x00000900\t[  1] __RINvNtCs1_4core3ptr9drop_glueNtCs2_13marspot_shell3FooECs2_13marspot_shell
0x100010000\t0x00001000\t[  1] literal string: hello
0x100020000\t0x00004000\t[  1] __ZN12marspot_term5STATE17h0123456789abcdefE

# Dead Stripped Symbols:
#        \tSize    \tFile  Name
<<dead>> \t0x00099999\t[  1] __ZN12marspot_term4dead17h0123456789abcdefE
"""


def _expect(cond, what, failures):
    if not cond:
        failures.append(what)


def _map_checks(work, failures):
    path = os.path.join(work, "x.map")
    open(path, "w").write(MAP)
    rows = _size_map.summarize(path, 0x20000)
    _expect(sum(rows.values()) == 0x20000, f"map rows add up to {sum(rows.values())}", failures)
    _expect(rows.get("marspot_term::parser") == 0x3000, f"parser row {rows}", failures)
    _expect(rows.get("marspot_term::grid") == 0x1000, f"impl row {rows}", failures)
    _expect(rows.get("core") == 0x2000, f"core row {rows}", failures)
    _expect(rows.get("marspot_shell::windows") == 0x900, f"v0 row {rows}", failures)
    _expect(rows.get("(other small)") == 0x100, f"small row {rows}", failures)
    _expect(rows.get("marspot_shell::plugins::claudecode") == 0x1000, f"plugin row {rows}", failures)
    _expect(rows.get("core <- marspot_shell") == 0x900, f"instance row {rows}", failures)
    _expect(rows.get("(__TEXT,__cstring)") == 0x1000, f"section row {rows}", failures)
    _expect("marspot_term::STATE" not in rows, "zerofill symbol counted", failures)
    _expect(not any("dead" in k for k in rows), "dead-stripped symbol counted", failures)
    return path


def _make_root(work, map_path):
    root = os.path.join(work, "root")
    os.makedirs(os.path.join(root, "bench"))
    open(os.path.join(root, "Cargo.toml"), "w").write('[package]\nname = "marspot"\nversion = "0.18.3"\n')
    json.dump({}, open(os.path.join(root, sb.BUDGET_FILE), "w"))
    binf = os.path.join(work, "bin")
    open(binf, "wb").write(b"\0" * 0x20000)
    for name in sb.BINARIES:
        raw = os.path.join(work, f"{name}.raw.tsv")
        sb.raw(map_path, binf, name, raw, root)
        sb.set_budget(name, 0x30000, raw, root)
    return root


def _digest(root):
    h = hashlib.sha256()
    for d, _, files in sorted(os.walk(os.path.join(root, "bench"))):
        if "results" in d:
            continue
        for fn in sorted(files):
            h.update(fn.encode() + open(os.path.join(d, fn), "rb").read())
    return h.hexdigest()


def _gate(root, work, size, update="0"):
    cur = os.path.join(work, "cur")
    shutil.rmtree(cur, ignore_errors=True)
    os.makedirs(cur)
    for name in sb.BINARIES:
        open(os.path.join(cur, f"size-{name}.txt"), "w").write(f"{size}\n")
    env = dict(os.environ, MARSPOT_BENCH_HISTORY_DIR=os.path.join(work, "history"))
    baseline = os.path.join(work, "baseline.json")
    if not os.path.exists(baseline):
        shutil.copy(os.path.join(HERE, "..", "bench", "baseline.json"), baseline)
    p = subprocess.run([sys.executable, os.path.join(HERE, "_bench_gate.py"),
                        baseline, cur, "fast", update, root],
                       env=env, capture_output=True, text=True)
    return p.returncode, p.stdout + p.stderr


def _rule_checks(root, failures):
    _expect(sb.check(root) == [], f"fresh budgets refused: {sb.check(root)}", failures)
    path = os.path.join(root, sb.BUDGET_FILE)
    good = open(path).read()
    b = json.loads(good)
    b["0.18"]["marspot-shell"]["budget"] += 16384
    json.dump(b, open(path, "w"))
    p = sb.check(root)
    _expect(any("table was written for" in x for x in p), f"raise without table accepted: {p}", failures)
    open(path, "w").write(good)
    table = sb.table_path(root, "0.18", "marspot-core")
    saved = open(table).read()
    os.remove(table)
    p = sb.check(root)
    _expect(any("has no size table" in x for x in p), f"missing table accepted: {p}", failures)
    open(table, "w").write(saved + "extra\t5\t0\t+5\n")
    p = sb.check(root)
    _expect(any("add up to" in x for x in p), f"table with wrong sum accepted: {p}", failures)
    open(table, "w").write(saved)
    _expect(sb.minor_in_force(json.loads(good), "0.19") == "0.18", "0.19 does not inherit 0.18", failures)
    _expect(sb.minor_in_force(json.loads(good), "0.17") is None, "0.17 picked up 0.18", failures)


def _gate_checks(root, work, failures):
    before = _digest(root)
    rc, out = _gate(root, work, 0x20000)
    _expect(rc == 0 and "size marspot-shell" in out and "FAIL" not in out,
            f"under budget: rc {rc}\n{out[-1500:]}", failures)
    _expect(_digest(root) == before, "an under-budget run changed a budget file", failures)
    hist = os.path.join(work, "history", sb.HISTORY_FILE)
    lines = open(hist).read().splitlines() if os.path.exists(hist) else []
    _expect(len(lines) == 2 and lines[0].split("\t") == list(sb.HISTORY_COLUMNS)
            and lines[1].split("\t")[2:6] == [str(0x20000)] * 4,
            f"size history: {lines}", failures)
    rc, out = _gate(root, work, 0x20000, update="1")
    _expect(rc == 0 and "wrote" in out and _digest(root) == before,
            f"--update-baseline touched a budget: rc {rc}\n{out[-800:]}", failures)
    _expect("binary_size_bytes_max" not in json.load(open(os.path.join(work, "baseline.json"))),
            "--update-baseline wrote a size ceiling", failures)
    rc, out = _gate(root, work, 0x30001)
    _expect(rc == 1 and "GATE FAILED" in out, f"over budget: rc {rc}\n{out[-1500:]}", failures)
    path = os.path.join(root, sb.BUDGET_FILE)
    good = open(path).read()
    b = json.loads(good)
    b["0.18"]["marspot-peer"]["budget"] = 0x40000
    json.dump(b, open(path, "w"))
    rc, out = _gate(root, work, 0x20000)
    _expect(rc == 1 and "budget refused" in out, f"refused budget: rc {rc}\n{out[-1500:]}", failures)
    open(path, "w").write(good)


def run():
    failures = []
    work = tempfile.mkdtemp(prefix="marspot-size-selftest-")
    try:
        root = _make_root(work, _map_checks(work, failures))
        _rule_checks(root, failures)
        _gate_checks(root, work, failures)
    finally:
        shutil.rmtree(work, ignore_errors=True)
    for f in failures:
        print(f"SELF-TEST FAIL: {f}")
    print("SIZE BUDGET SELF-TEST " + ("FAIL" if failures else "PASS"))
    return 1 if failures else 0
