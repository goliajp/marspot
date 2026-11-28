"""The part of TOML a Codex config.toml uses here: tables (dotted and
quoted names), `key = value` with strings, numbers, booleans, arrays
and one-line inline tables, and comments.  The python3 a runner has on
PATH may predate tomllib."""
import re

_BARE = re.compile(r"[A-Za-z0-9_-]+")


def _key(s, i):
    """A dotted key from s[i:]: (parts, next index)."""
    parts = []
    while True:
        while i < len(s) and s[i] in " \t":
            i += 1
        if i < len(s) and s[i] in "\"'":
            v, i = _string(s, i)
            parts.append(v)
        else:
            m = _BARE.match(s, i)
            if not m:
                raise ValueError("bad key at %r" % s[i:i + 20])
            parts.append(m.group(0))
            i = m.end()
        while i < len(s) and s[i] in " \t":
            i += 1
        if i < len(s) and s[i] == ".":
            i += 1
            continue
        return parts, i


def _string(s, i):
    q = s[i]
    j = i + 1
    out = []
    while s[j] != q:
        if q == '"' and s[j] == "\\":
            esc = s[j + 1]
            out.append({"n": "\n", "t": "\t", '"': '"', "\\": "\\", "r": "\r"}.get(esc, esc))
            j += 2
        else:
            out.append(s[j])
            j += 1
    return "".join(out), j + 1


def value(s, i=0):
    """A value from s[i:]: (value, next index)."""
    while s[i] in " \t":
        i += 1
    c = s[i]
    if c in "\"'":
        return _string(s, i)
    if c == "[":
        out, i = [], i + 1
        while True:
            while s[i] in " \t\n,":
                i += 1
            if s[i] == "]":
                return out, i + 1
            v, i = value(s, i)
            out.append(v)
    if c == "{":
        out, i = {}, i + 1
        while True:
            while s[i] in " \t,":
                i += 1
            if s[i] == "}":
                return out, i + 1
            parts, i = _key(s, i)
            i = s.index("=", i) + 1
            v, i = value(s, i)
            set_path(out, parts, v)
    m = re.compile(r"[^,\]}\s#]+").match(s, i)
    word = m.group(0)
    if word in ("true", "false"):
        return word == "true", m.end()
    try:
        return (float(word) if "." in word else int(word)), m.end()
    except ValueError:
        return word, m.end()


def set_path(d, parts, v):
    for p in parts[:-1]:
        d = d.setdefault(p, {})
    d[parts[-1]] = v


def loads(text):
    root, table = {}, {}
    table = root
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("["):
            parts, _ = _key(line.strip("[]"), 0)
            table = root
            for p in parts:
                table = table.setdefault(p, {})
            continue
        parts, i = _key(line, 0)
        i = line.index("=", i) + 1
        v, _ = value(line, i)
        set_path(table, parts, v)
    return root


def override(cfg, kv):
    """Apply one `-c key=value`: the value is TOML, or a bare string."""
    k, _, v = kv.partition("=")
    parts, _ = _key(k.strip(), 0)
    try:
        val, _ = value(v.strip())
    except (ValueError, IndexError):
        val = v.strip()
    set_path(cfg, parts, val)
