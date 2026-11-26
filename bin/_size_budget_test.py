#!/usr/bin/env python3
"""`_size_budget.py --self-test`: the size budget rules can fail and can pass.

Builds a throwaway root (Cargo.toml, bench/size-budget.json, the size
tables) from a fabricated link map, then checks the map summary, the
matching rule, and the bench gate itself on fabricated size readings:
under budget passes and leaves the budget files alone, one binary
over budget fails, a missing reading has no verdict, and a budget
raised without its table, by editing the table header, or without the
build needing it is refused.
"""
import hashlib
import json
import os
import re
import shutil
import struct
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


LINKEDIT = 0x1800


def _fake_macho(path, size):
    """A file of `size` bytes whose only load command is a __LINKEDIT of LINKEDIT bytes."""
    seg = struct.pack("<2I16s4Q4I", 0x19, 72, b"__LINKEDIT", 0, 0, size - LINKEDIT, LINKEDIT, 1, 1, 0, 0)
    head = struct.pack("<8I", 0xFEEDFACF, 0x0100000C, 0, 2, 1, len(seg), 0, 0)
    open(path, "wb").write((head + seg).ljust(size, b"\0"))


def _expect(cond, what, failures):
    if not cond:
        failures.append(what)


def _map_checks(work, failures):
    path = os.path.join(work, "x.map")
    open(path, "w").write(MAP)
    binf = os.path.join(work, "bin")
    _fake_macho(binf, 0x20000)
    rows = _size_map.summarize(path, 0x20000, _size_map.linkedit_bytes(binf))
    _expect(sum(rows.values()) == 0x20000, f"map rows add up to {sum(rows.values())}", failures)
    _expect(rows.get(_size_map.LINKEDIT) == LINKEDIT, f"linkedit row {rows}", failures)
    _expect(rows.get(_size_map.UNMAPPED) == 0x20000 - 0x9300 - LINKEDIT, f"unmapped row {rows}", failures)
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


def _gate(root, work, sizes, update="0"):
    """Run the gate on size readings {binary: bytes}; a binary left out has no reading."""
    cur = os.path.join(work, "cur")
    shutil.rmtree(cur, ignore_errors=True)
    os.makedirs(cur)
    for name, n in sizes.items():
        open(os.path.join(cur, f"size-{name}.txt"), "w").write(f"{n}\n")
    env = dict(os.environ, MARSPOT_BENCH_HISTORY_DIR=os.path.join(work, "history"))
    baseline = os.path.join(work, "baseline.json")
    if not os.path.exists(baseline):
        shutil.copy(os.path.join(HERE, "..", "bench", "baseline.json"), baseline)
    p = subprocess.run([sys.executable, os.path.join(HERE, "_bench_gate.py"),
                        baseline, cur, "fast", update, root],
                       env=env, capture_output=True, text=True)
    return p.returncode, p.stdout + p.stderr


def _all(n):
    return {name: n for name in sb.BINARIES}


class _Undo:
    """Puts the budget file, the tables and Cargo.toml back after a case edits them."""

    def __init__(self, root):
        self.root, self.saved = root, os.path.join(root, "..", "saved")

    def __enter__(self):
        shutil.rmtree(self.saved, ignore_errors=True)
        shutil.copytree(os.path.join(self.root, "bench"), self.saved)
        self.cargo = open(os.path.join(self.root, "Cargo.toml")).read()
        return self

    def __exit__(self, *_):
        shutil.rmtree(os.path.join(self.root, "bench"))
        shutil.copytree(self.saved, os.path.join(self.root, "bench"))
        open(os.path.join(self.root, "Cargo.toml"), "w").write(self.cargo)


def _edit_budget(root, name, budget, minor="0.18"):
    path = os.path.join(root, sb.BUDGET_FILE)
    b = json.load(open(path))
    b.setdefault(minor, {})[name] = dict(b["0.18"][name], budget=budget)
    json.dump(b, open(path, "w"))


def _edit_table(root, name, fn):
    table = sb.table_path(root, "0.18", name)
    lines = open(table).readlines()
    open(table, "w").write("".join(fn(line) for line in lines))


def _refused(root, needle, what, failures):
    p = sb.check(root)
    _expect(any(needle in x for x in p), f"{what} accepted: {p}", failures)


def _table_checks(root, failures):
    with _Undo(root):
        _edit_budget(root, "marspot-shell", 0x30000 + 16384)
        _refused(root, "table was written for", "raise without table", failures)
    with _Undo(root):
        _edit_budget(root, "marspot-shell", 0x40000)
        _edit_table(root, "marspot-shell", lambda l: "# budget\t262144\n" if l.startswith("# budget\t") else l)
        _refused(root, "checksum does not hold", "raise by editing the table header", failures)
    with _Undo(root):
        os.remove(sb.table_path(root, "0.18", "marspot-core"))
        _refused(root, "has no size table", "missing table", failures)
    with _Undo(root):
        open(sb.table_path(root, "0.18", "marspot-core"), "a").write("extra\t5\t0\t+5\n")
        _refused(root, "add up to", "table with wrong sum", failures)
    with _Undo(root):
        head, rows = sb.read_table(sb.table_path(root, "0.18", "marspot-core"))
        over = rows[_size_map.UNMAPPED] + 7
        rows["core"] += over
        rows[_size_map.UNMAPPED] -= over
        head["checksum"] = sb.checksum(head, rows)
        sb.write_table(sb.table_path(root, "0.18", "marspot-core"), head, rows, {})
        _refused(root, "more than the file", "negative unmapped row", failures)
    with _Undo(root):
        open(os.path.join(root, "Cargo.toml"), "w").write('[package]\nname = "marspot"\nversion = "0.17.9"\n')
        _refused(root, "no budget for product version 0.17", "version below every key", failures)


def _raise_checks(root, work, failures):
    with _Undo(root):
        p = sb.set_budget("marspot-peer", 0x40000, os.path.join(work, "marspot-peer.raw.tsv"), root)
        _expect(any("must be needed by the build" in x for x in p), f"unneeded raise accepted: {p}", failures)
        _expect(sb.check(root) == [], "a refused set changed the tree", failures)
        p = sb.set_budget("marspot-peer", 0x40000, os.path.join(work, "marspot-peer.raw.tsv"), root)
        _expect(any("must be needed by the build" in x for x in p), f"a second set raised it: {p}", failures)
    with _Undo(root):
        _edit_table(root, "marspot-peer", lambda l: f"# budget\t{0x40000}\n" if l.startswith("# budget\t") else l)
        p = sb.set_budget("marspot-peer", 0x40000, os.path.join(work, "marspot-peer.raw.tsv"), root)
        _expect(any("edited since" in x for x in p), f"table edit then set raised it: {p}", failures)
    with _Undo(root):
        bad = os.path.join(work, "badsum.tsv")
        text = open(os.path.join(work, "marspot-peer.raw.tsv")).read()
        open(bad, "w").write(re.sub(r"# file_bytes\t\d+", f"# file_bytes\t{0x38000}", text))
        p = sb.set_budget("marspot-peer", 0x38000, bad, root)
        _expect(any("add up to" in x for x in p), f"rows not adding up accepted: {p}", failures)
        _expect(sb.check(root) == [], "a set refused by its table changed the tree", failures)
    with _Undo(root):
        p = sb.set_budget("marspot-peer", 0x100, os.path.join(work, "marspot-peer.raw.tsv"), root)
        _expect(any("under the measured" in x for x in p), f"lower under measured accepted: {p}", failures)
        _expect(sb.check(root) == [], "a refused lower changed the tree", failures)
    with _Undo(root):
        b = json.load(open(os.path.join(root, sb.BUDGET_FILE)))
        del b["0.18"]["marspot-peer"]
        json.dump(b, open(os.path.join(root, sb.BUDGET_FILE), "w"))
        open(os.path.join(root, "Cargo.toml"), "w").write('[package]\nname = "marspot"\nversion = "0.19.0"\n')
        raw19 = os.path.join(work, "peer19.tsv")
        open(raw19, "w").write(open(os.path.join(work, "marspot-peer.raw.tsv")).read().replace("0.18.", "0.19."))
        p = sb.set_budget("marspot-peer", 0x40000, raw19, root)
        _expect(any("must be needed by the build" in x for x in p), f"json delete then set at 0.19: {p}", failures)
    with _Undo(root):
        _edit_budget(root, "marspot-peer", 0x40000)
        p = sb.set_budget("marspot-peer", 0x40000, os.path.join(work, "marspot-peer.raw.tsv"), root)
        _expect(any("must be needed by the build" in x for x in p), f"json edit then set raised it: {p}", failures)
    with _Undo(root):
        big = os.path.join(work, "big")
        _fake_macho(big, 0x38000)
        sb.raw(os.path.join(work, "x.map"), big, "marspot-peer", os.path.join(work, "big.tsv"), root)
        p = sb.set_budget("marspot-peer", 0x40000, os.path.join(work, "big.tsv"), root)
        _expect(p == [], f"needed raise refused: {p}", failures)
    with _Undo(root):
        os.makedirs(os.path.join(root, sb.TABLE_DIR, "0.19"))
        shutil.copy(sb.table_path(root, "0.18", "marspot-peer"), sb.table_path(root, "0.19", "marspot-peer"))
        _edit_budget(root, "marspot-peer", 0x30000, minor="0.19")
        _refused(root, "previous_budget none, but 0.18", "a second first budget", failures)


def _rule_checks(root, work, failures):
    _expect(sb.check(root) == [], f"fresh budgets refused: {sb.check(root)}", failures)
    _table_checks(root, failures)
    _raise_checks(root, work, failures)
    _expect(sb.check(root) == [], f"a case did not put the budgets back: {sb.check(root)}", failures)
    b = sb.load(root)
    _expect(sb.minor_in_force(b, "0.19") == "0.18", "0.19 does not inherit 0.18", failures)
    _expect(sb.minor_in_force(b, "0.17") is None, "0.17 picked up 0.18", failures)


def _gate_checks(root, work, failures):
    before = _digest(root)
    rc, out = _gate(root, work, _all(0x20000))
    _expect(rc == 0 and "size marspot-shell" in out and "FAIL" not in out,
            f"under budget: rc {rc}\n{out[-1500:]}", failures)
    _expect(_digest(root) == before, "an under-budget run changed a budget file", failures)
    hist = os.path.join(work, "history", sb.HISTORY_FILE)
    lines = open(hist).read().splitlines() if os.path.exists(hist) else []
    _expect(len(lines) == 2 and lines[0].split("\t") == list(sb.HISTORY_COLUMNS)
            and lines[1].split("\t")[2:6] == [str(0x20000)] * 4,
            f"size history: {lines}", failures)
    rc, out = _gate(root, work, _all(0x20000), update="1")
    _expect(rc == 0 and "wrote" in out and _digest(root) == before,
            f"--update-baseline touched a budget: rc {rc}\n{out[-800:]}", failures)
    _expect("binary_size_bytes_max" not in json.load(open(os.path.join(work, "baseline.json"))),
            "--update-baseline wrote a size ceiling", failures)
    rc, out = _gate(root, work, dict(_all(0x20000), **{"marspot-peer": 0x30001}))
    peer = [l for l in out.splitlines() if l.startswith("size marspot-peer")]
    _expect(rc == 1 and "GATE FAILED (1 regression" in out and peer and "FAIL" in peer[0],
            f"marspot-peer alone over budget: rc {rc}\n{out[-1500:]}", failures)
    missing = _all(0x20000)
    del missing["marspot-core"]
    rc, out = _gate(root, work, missing)
    _expect(rc == 3, f"no reading for marspot-core: rc {rc}\n{out[-1500:]}", failures)
    with _Undo(root):
        _edit_budget(root, "marspot-peer", 0x40000)
        rc, out = _gate(root, work, _all(0x20000))
        _expect(rc == 1 and "budget refused" in out, f"refused budget: rc {rc}\n{out[-1500:]}", failures)


def run():
    failures = []
    work = tempfile.mkdtemp(prefix="marspot-size-selftest-")
    try:
        root = _make_root(work, _map_checks(work, failures))
        _rule_checks(root, work, failures)
        _gate_checks(root, work, failures)
    finally:
        shutil.rmtree(work, ignore_errors=True)
    for f in failures:
        print(f"SELF-TEST FAIL: {f}")
    print("SIZE BUDGET SELF-TEST " + ("FAIL" if failures else "PASS"))
    return 1 if failures else 0
