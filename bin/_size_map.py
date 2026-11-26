#!/usr/bin/env python3
"""Sum an ld64 link map (`-Wl,-map,<file>`) by crate and module.

  _size_map.py <map> <binary>      print "<module>\t<bytes>" rows

Fat LTO puts every Rust crate into one object file, so the object a
symbol came from says nothing.  The symbol name does: a mangled Rust
name starts with its crate, then its module.  Workspace crates are
split one level further (`marspot_term::parser`); every other crate is
one row.  A symbol that is not Rust (C, Objective-C stubs, anonymous
constants, literal strings) is counted under the section it lives in.

The rows always add up to the binary's file size: what no symbol
covers (Mach-O headers, `__LINKEDIT`, the code signature, page
padding) is one row of its own, so a size table cannot silently leave
part of the binary out.
"""
import os
import re
import sys

WORKSPACE = {
    "marspot", "marspot_shell", "marspot_core", "marspot_term", "marspot_session",
    "marspot_peer", "marspot_linkify", "golia_ui_core",
}
# zerofill sections take address space, not file bytes
ZEROFILL = {"__bss", "__common", "__thread_bss"}
SMALL = 2048
UNMAPPED = "(unmapped: headers, linkedit, signature, padding)"

_LEGACY_IDENT = re.compile(r"(\d+)")
_SYMBOL = re.compile(r"^0x([0-9A-Fa-f]+)\s+0x([0-9A-Fa-f]+)\s+\[\s*\d+\]\s(.*)$")
_SECTION = re.compile(r"^0x([0-9A-Fa-f]+)\s+0x([0-9A-Fa-f]+)\s+(\S+)\s+(\S+)\s*$")
_IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def _legacy_parts(body):
    """Identifiers of a legacy `_ZN<len><ident>...E` name, outermost first."""
    parts, i = [], 0
    while i < len(body) and body[i].isdigit():
        m = _LEGACY_IDENT.match(body, i)
        n = int(m.group(1))
        start = m.end()
        parts.append(body[start:start + n])
        i = start + n
    return parts


def _impl_self(ident):
    """`_$LT$marspot_term..grid..Grid$u20$as$u20$...` -> ["marspot_term", "grid"]."""
    s = ident[len("_$LT$"):]
    for prefix in ("$RF$", "$BP$", "$u5b$", "_"):
        while s.startswith(prefix):
            s = s[len(prefix):]
    s = re.sub(r"^(mut\$u20\$|dyn\$u20\$)", "", s)
    path = re.split(r"\$|\s", s, maxsplit=1)[0]
    return [p for p in path.split("..") if p]


def _v0_parts(name):
    """Crate and the identifiers after it in a v0 `_R...` name, outermost first."""
    m = re.search(r"C(?:s[0-9A-Za-z]*_)?(\d+)", name)
    if not m:
        return []
    parts, i = [], m.start(1)
    while len(parts) < 3:
        d = re.match(r"(?:s[0-9A-Za-z]*_)?(\d+)_?", name[i:])
        if not d:
            break
        start = i + d.end()
        parts.append(name[start:start + int(d.group(1))])
        i = start + int(d.group(1))
    return parts


def _v0_instantiator(name):
    """The crate a v0 generic instance was instantiated in (its trailing crate root)."""
    for m in re.finditer(r"\d+", name):
        n = int(m.group())
        if m.end() + n == len(name) and re.search(r"C(?:s[0-9A-Za-z]*_)?$", name[:m.start()]):
            return name[m.end():]
    return None


def module_of(symbol):
    """`crate::module` for a workspace crate, `crate` for another, None if not Rust.

    `marspot_shell::plugins` gets one level more.  A generic from a
    crate outside the workspace instantiated in a workspace crate is
    `core <- marspot_shell`: that crate's use of it is what grew.
    """
    s = symbol.lstrip("_")
    inst = None
    if s.startswith("ZN"):
        parts = _legacy_parts(s[2:])
        if parts and parts[0].startswith("_$LT$"):
            parts = _impl_self(parts[0]) + parts[1:]
    elif s.startswith("R"):
        parts = _v0_parts(s)
        inst = _v0_instantiator(s)
    else:
        return None
    parts = [p for p in parts if _IDENT.fullmatch(p) and not re.fullmatch(r"h[0-9a-f]{16}", p)]
    if not parts:
        return None
    if parts[0] not in WORKSPACE:
        return f"{parts[0]} <- {inst}" if inst in WORKSPACE else parts[0]
    depth = 3 if parts[:2] == ["marspot_shell", "plugins"] else 2
    return "::".join(parts[:depth])


def summarize(map_path, file_bytes):
    """{row: bytes}, adding up to `file_bytes`."""
    sections, totals, mode = [], {}, None
    for line in open(map_path, errors="replace"):
        line = line.rstrip("\n")
        if line.startswith("#"):
            head = line[1:].strip()
            if head.startswith("Sections:"):
                mode = "sections"
            elif head.startswith("Symbols:"):
                mode = "symbols"
            elif head.startswith("Dead Stripped Symbols"):
                break
            continue
        if mode == "sections":
            m = _SECTION.match(line)
            if m:
                sections.append((int(m.group(1), 16), int(m.group(2), 16), m.group(3), m.group(4)))
        elif mode == "symbols":
            m = _SYMBOL.match(line)
            if not m:
                continue
            addr, size, name = int(m.group(1), 16), int(m.group(2), 16), m.group(3)
            sec = next((s for s in sections if s[0] <= addr < s[0] + max(s[1], 1)), None)
            if sec and sec[3] in ZEROFILL:
                continue
            key = module_of(name) or (f"({sec[2]},{sec[3]})" if sec else "(no section)")
            totals[key] = totals.get(key, 0) + size
    return _fold_small(totals, file_bytes)


def _fold_small(totals, file_bytes):
    rows = {}
    for key, n in totals.items():
        if n < SMALL:
            crate = key.split("::")[0]
            key = f"{crate}::(small)" if crate in WORKSPACE else "(other small)"
        rows[key] = rows.get(key, 0) + n
    rows[UNMAPPED] = file_bytes - sum(rows.values())
    return rows


def main(argv):
    if len(argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    rows = summarize(argv[1], os.path.getsize(argv[2]))
    for key, n in sorted(rows.items(), key=lambda kv: (-kv[1], kv[0])):
        print(f"{key}\t{n}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
