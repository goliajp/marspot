#!/usr/bin/env python3
"""Binary size budgets: one absolute budget per binary per product minor version.

  _size_budget.py binaries                 the binaries that carry a budget
  _size_budget.py check [root]             is every budget backed by its table
  _size_budget.py raw <map> <bin-file> <name> <out.tsv> [root]
                                           module table from a link map, no budget yet
  _size_budget.py set <name> <budget> <raw.tsv> [root]
                                           write the budget and its size-change table
  _size_budget.py --self-test

Budgets live in bench/size-budget.json:

  {"0.18": {"marspot-shell": {"budget": 7340032, "measured": 6362976}, ...}}

The key is the product minor version (Cargo.toml [package] version
without the patch).  A version with no key of its own runs under the
newest lower one.  Under its budget a bench run changes no tracked
file; over it the size row fails.  Nothing measures its way into a
budget: `--update-baseline` does not touch it.

A budget is accepted only with its table, and the bench refuses the
whole set when one does not match.  The table for minor M and binary
B is bench/size-tables/M/B.tsv, and it matches when its header names
B and a version of minor M, its `budget` equals the budget, its
`file_bytes` equals the entry's `measured`, its module rows add up to
`file_bytes`, and `measured` is not over `budget`.  So raising a
budget is one commit with the new number and the table of the build
that needed it: `bin/bench-remote.sh --size-tables` links the four
binaries with a map on the bench host and brings back raw tables in
bench/remote-runs/<ts>/size-tables/; `set` turns one into the
committed table, with the change per module against the table it
replaces.

Every run's sizes also go to <history-dir>/size-history.tsv (see
append_history), which is not tracked.
"""
import json
import os
import re
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _size_map  # noqa: E402

BINARIES = ("marspot-shell", "marspot-core", "marspot-session", "marspot-peer")
BUDGET_FILE = os.path.join("bench", "size-budget.json")
TABLE_DIR = os.path.join("bench", "size-tables")
HISTORY_FILE = "size-history.tsv"
HISTORY_COLUMNS = ("ts", "commit") + BINARIES + ("shell_prod_lines",)
HISTORY_KEEP = 5000
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def product_version(root):
    text = open(os.path.join(root, "Cargo.toml")).read()
    pkg = text.split("[package]", 1)[1].split("\n[", 1)[0]
    return re.search(r'^version\s*=\s*"([^"]+)"', pkg, re.M).group(1)


def minor_of(version):
    return ".".join(version.split(".")[:2])


def _minor_key(minor):
    return tuple(int(x) for x in minor.split("."))


def load(root):
    return json.load(open(os.path.join(root, BUDGET_FILE)))


def minor_in_force(budgets, minor):
    """The budget key a product minor runs under: its own, else the newest lower one."""
    keys = [k for k in budgets if re.fullmatch(r"\d+\.\d+", k)]
    lower = [k for k in keys if _minor_key(k) <= _minor_key(minor)]
    return max(lower, key=_minor_key) if lower else None


def table_path(root, minor, name):
    return os.path.join(root, TABLE_DIR, minor, f"{name}.tsv")


def read_table(path):
    head, rows = {}, {}
    for line in open(path):
        line = line.rstrip("\n")
        if line.startswith("# "):
            k, _, v = line[2:].partition("\t")
            head[k] = v
        elif line and not line.startswith("module\t"):
            cols = line.split("\t")
            rows[cols[0]] = int(cols[1])
    return head, rows


def check(root):
    """Problems with the budgets in bench/size-budget.json; empty when every one matches."""
    budgets = load(root)
    problems = []
    for minor, entries in budgets.items():
        if minor.startswith("_"):
            continue
        if not re.fullmatch(r"\d+\.\d+", minor):
            problems.append(f"{minor!r} is not a product minor version")
            continue
        for name, e in entries.items():
            problems += _check_entry(root, minor, name, e)
    current = minor_of(product_version(root))
    in_force = minor_in_force(budgets, current)
    for name in BINARIES:
        if in_force is None or name not in budgets[in_force]:
            problems.append(f"{name} has no budget for product version {current}")
    return problems


def _check_entry(root, minor, name, e):
    where = f"{name} {minor}"
    if not (isinstance(e, dict) and isinstance(e.get("budget"), int)
            and isinstance(e.get("measured"), int)):
        return [f"{where}: entry needs integer budget and measured"]
    path = table_path(root, minor, name)
    if not os.path.exists(path):
        return [f"{where}: budget {e['budget']} has no size table ({os.path.relpath(path, root)})"]
    head, rows = read_table(path)
    out = []
    if head.get("binary") != name:
        out.append(f"{where}: table is for {head.get('binary')!r}")
    if minor_of(head.get("version", "")) != minor:
        out.append(f"{where}: table is for version {head.get('version')!r}")
    if head.get("budget") != str(e["budget"]):
        out.append(f"{where}: budget {e['budget']} but its table was written for {head.get('budget')!r}")
    if head.get("file_bytes") != str(e["measured"]):
        out.append(f"{where}: measured {e['measured']} but its table read {head.get('file_bytes')!r}")
    elif sum(rows.values()) != e["measured"]:
        out.append(f"{where}: table rows add up to {sum(rows.values())}, not {e['measured']}")
    if e["measured"] > e["budget"]:
        out.append(f"{where}: measured {e['measured']} is over its own budget {e['budget']}")
    return out


def write_table(path, head, rows, previous):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    keys = sorted(set(rows) | set(previous), key=lambda k: (-rows.get(k, 0), k))
    with open(path, "w") as f:
        for k in ("binary", "version", "commit", "file_bytes", "budget", "previous"):
            f.write(f"# {k}\t{head.get(k, '-')}\n")
        f.write("module\tbytes\tprevious\tdelta\n")
        for k in keys:
            a, b = rows.get(k, 0), previous.get(k, 0)
            f.write(f"{k}\t{a}\t{b}\t{a - b:+d}\n")


def raw(map_path, bin_path, name, out, root):
    file_bytes = os.path.getsize(bin_path)
    rows = _size_map.summarize(map_path, file_bytes)
    commit = os.environ.get("MARSPOT_BENCH_COMMIT") or "-"
    head = {"binary": name, "version": product_version(root), "commit": commit,
            "file_bytes": file_bytes}
    write_table(out, head, rows, {})


def set_budget(name, budget, raw_path, root):
    head, rows = read_table(raw_path)
    if head.get("binary") != name:
        raise SystemExit(f"{raw_path} is a table for {head.get('binary')!r}, not {name}")
    minor = minor_of(head["version"])
    budgets = load(root)
    prev_minor = minor_in_force(budgets, minor)
    prev_path = table_path(root, prev_minor, name) if prev_minor else None
    previous = {}
    if prev_path and os.path.exists(prev_path):
        prev_head, previous = read_table(prev_path)
        head["previous"] = f"{prev_minor} {prev_head.get('file_bytes')}"
    head["budget"] = budget
    write_table(table_path(root, minor, name), head, rows, previous)
    budgets.setdefault(minor, {})[name] = {"budget": budget, "measured": int(head["file_bytes"])}
    with open(os.path.join(root, BUDGET_FILE), "w") as f:
        json.dump(budgets, f, indent=2)
        f.write("\n")
    return check(root)


def copy_budgets(src_root, dst_root):
    """Give a throwaway gate root the budgets of a tree: the size rows read them from where they run."""
    import shutil
    shutil.copytree(os.path.join(src_root, TABLE_DIR), os.path.join(dst_root, TABLE_DIR))
    for f in ("Cargo.toml", BUDGET_FILE):
        shutil.copy(os.path.join(src_root, f), os.path.join(dst_root, f))


def add_rows(cur_dir, root, row, samples):
    """The size rows of the bench gate.  A budget set the tables do not
    back is refused: every size row fails and says why."""
    problems = check(root)
    budgets = load(root)
    in_force = minor_in_force(budgets, minor_of(product_version(root)))
    entries = budgets.get(in_force, {}) if in_force else {}
    for p in problems:
        print(f"size budget refused: {p}")
    for name in BINARIES:
        xs = samples(f"size-{name}.txt", int)
        limit = entries.get(name, {}).get("budget", 0)
        # a budget with no reading is a check that did not run, not a pass
        r = row(f"size {name}", xs[0] if xs else None, "bytes", limit, True, trials=xs,
                stat="exact", host=None if xs else {"invalid": f"no size reading for {name}: not built?"})
        if problems:
            r.update(verdict="fail", note=f"budget refused: {problems[0]}")


def shell_prod_lines(root):
    """Lines of L1 production code: test files and trailing #[cfg(test)] blocks left out."""
    total = 0
    for d, _, files in os.walk(os.path.join(root, "src", "bin", "marspot-shell")):
        if "tests" in d[len(root):].split(os.sep):
            continue
        for fn in files:
            if not fn.endswith(".rs") or fn == "tests.rs" or fn.endswith("_tests.rs"):
                continue
            for line in open(os.path.join(d, fn), errors="replace"):
                if line.startswith("#[cfg(test)]"):
                    break
                total += 1
    return total


def append_history(record, hist_dir):
    """One line per run with any size reading: ts, commit, the four sizes, L1 production lines."""
    sizes = {r["metric"][5:]: r.get("value") for r in record.get("rows", [])
             if r["metric"].startswith("size ")}
    if not sizes:
        return
    path = os.path.join(hist_dir, HISTORY_FILE)
    lines = open(path).read().splitlines()[1:] if os.path.exists(path) else []
    vals = [record.get("ts"), record.get("commit")] + [sizes.get(b) for b in BINARIES]
    vals.append(record.get("shell_prod_lines"))
    lines.append("\t".join("" if v is None else str(v) for v in vals))
    os.makedirs(hist_dir, exist_ok=True)
    with open(path, "w") as f:
        f.write("\t".join(HISTORY_COLUMNS) + "\n" + "\n".join(lines[-HISTORY_KEEP:]) + "\n")


def main(argv):
    cmd = argv[1] if len(argv) > 1 else ""
    if cmd == "binaries":
        print(" ".join(BINARIES))
    elif cmd == "check":
        problems = check(argv[2] if len(argv) > 2 else ROOT)
        for p in problems:
            print(f"size budget refused: {p}")
        print("size budgets: " + ("REFUSED" if problems else "every budget has its table"))
        return 1 if problems else 0
    elif cmd == "raw" and len(argv) in (6, 7):
        raw(argv[2], argv[3], argv[4], argv[5], argv[6] if len(argv) > 6 else ROOT)
    elif cmd == "set" and len(argv) in (5, 6):
        problems = set_budget(argv[2], int(argv[3]), argv[4], argv[5] if len(argv) > 5 else ROOT)
        for p in problems:
            print(f"size budget refused: {p}")
        return 1 if problems else 0
    elif cmd == "--self-test":
        import _size_budget_test
        return _size_budget_test.run()
    else:
        print(__doc__, file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
