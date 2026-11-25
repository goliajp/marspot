#!/usr/bin/env python3
"""bin/_active_result.py -- the result files of bin/measure-active.sh.

  one RUN TERM IDLE RC VERSION ONSCREEN FRONT OUT
      one reading: bin/measure-idle.sh's window (RUN/idle.json), the
      stream's own account of what it played (RUN/stream.json) and how
      much of it fell inside the window, the CPU split by process kind
      (names from RUN/ps.txt), every pane's size, the font and the window
  summary RESULTS "4 12"
      every reading of the run side by side, in RESULTS/active-vs-iterm.json;
      exits 2 when the two terminals of one N were not given the same
      work: stream bytes inside the window more than 2% apart, or active
      panes of sizes further apart than the fonts force plus
      ACTIVE_SIZE_SLACK cells
  font TERM ROOT
      the grid font of marspot (from the source) or iTerm2 (its default
      profile) and its cell size, as JSON
"""
import datetime
import json
import os
import platform
import plistlib
import re
import socket
import subprocess
import sys

APPS = {"marspot": "marspot-dev", "iterm2": "iterm2"}
BYTES_TOLERANCE = 0.02


def kind(cmd):
    if "iTermServer" in cmd:
        return "iTermServer"
    if "pidinfo" in cmd:
        return "pidinfo"
    if "MacOS/iTerm2" in cmd:
        return "iTerm2"
    m = re.search(r"/(marspot-(?:shell|core|session))( |$)", cmd)
    return m.group(1) if m else cmd.split(" ", 1)[0].rsplit("/", 1)[-1]


def read(path):
    with open(path) as f:
        return json.load(f)


def sizes(d):
    out = {}
    if os.path.isdir(d):
        for name in os.listdir(d):
            if name.isdigit():
                rows, cols = open(os.path.join(d, name)).read().split()
                out[name] = {"rows": int(rows), "cols": int(cols)}
    return out


def in_window(stream, t0, t1):
    """bytes the stream had written by t1 minus those by t0"""
    def by(t):
        b = 0
        for when, cum in stream["series"]:
            if when > t:
                break
            b = cum
        return b
    return by(t1) - by(t0)


def font(run):
    return read(os.path.join(run, "font"))


JXA = """ObjC.import('AppKit')
function run(a) {
  var f = $.NSFont.fontWithNameSize(a[0], parseFloat(a[1]))
  if (f.isNil()) return ''
  return f.maximumAdvancement.width + ' ' + (f.ascender - f.descender + f.leading)
}"""


def font_of(term, root):
    """the font a terminal draws its grid with, and its cell: AppKit's
    metrics times the terminal's own spacing"""
    hs = vs = 1.0
    if term == "marspot":
        src = open(os.path.join(root, "src", "font_cache.rs")).read()
        name = re.search(r'^pub const FONT_NAME: &str = "(.*)";', src, re.M).group(1)
        point = float(re.search(r"^pub const FONT_POINT: f64 = (.*);", src, re.M).group(1))
        origin = "src/font_cache.rs"
    else:
        p = subprocess.run(["defaults", "export", "com.googlecode.iterm2", "-"], capture_output=True)
        prefs = plistlib.loads(p.stdout) if p.returncode == 0 and p.stdout else {}
        guid = prefs.get("Default Bookmark Guid")
        b = next((x for x in prefs.get("New Bookmarks", []) if x.get("Guid") == guid), None)
        if b:
            name, _, point = b["Normal Font"].rpartition(" ")
            point, hs, vs = float(point), float(b.get("Horizontal Spacing", 1)), float(b.get("Vertical Spacing", 1))
            origin = "iTerm2 prefs, default profile"
        else:
            # what a fresh iTerm2 uses before anyone saves a profile
            name, point, origin = "Monaco", 12.0, "iTerm2 built-in default profile"
    out = subprocess.run(["/usr/bin/osascript", "-l", "JavaScript", "-e", JXA, name, str(point)],
                         capture_output=True, text=True).stdout.split()
    if len(out) != 2:
        print(f"no font {name} {point}", file=sys.stderr)
        return 2
    json.dump({"name": name, "point": point, "cell_w": round(float(out[0]) * hs, 3),
               "cell_h": round(float(out[1]) * vs, 3), "spacing": [hs, vs], "source": origin}, sys.stdout)
    return 0


def one(run, term, idle, rc, version, onscreen, front, out):
    doc = read(os.path.join(run, "idle.json"))
    stream_path = os.path.join(run, "stream.json")
    stream = read(stream_path) if os.path.exists(stream_path) else None
    procs = {}
    with open(os.path.join(run, "ps.txt")) as f:
        for line in f:
            pid, ppid, cmd = (line.strip().split(None, 2) + [""])[:3]
            procs[int(pid)] = (int(ppid), cmd.strip())
    w = doc["window_secs"]
    layers = {}
    for p in doc["per_process"]:
        ppid, cmd = procs.get(p["pid"], (None, "?"))
        p["kind"], p["ppid"] = kind(cmd), ppid
        l = layers.setdefault(p["kind"], {"processes": 0, "cpu_secs": 0.0, "rss_kib": 0})
        l["processes"] += 1
        l["cpu_secs"] = round(l["cpu_secs"] + p["cpu_secs"], 2)
        l["rss_kib"] += p["rss_kib"]
    for l in layers.values():
        l["cpu_percent_of_one_core"] = round(100.0 * l["cpu_secs"] / w, 2)
    active = open(os.path.join(run, "active")).read().strip()
    before, after = sizes(os.path.join(run, "sizes")), sizes(os.path.join(run, "sizes-after"))
    grid_path = os.path.join(run, "grid")
    if stream:
        stream["bytes_in_window"] = in_window(stream, doc["window_start_epoch"], doc["window_end_epoch"])
        series = stream.pop("series")
        stream["series_points"] = len(series)
    doc.update({
        "terminal": term,
        "terminal_version": version or None,
        "idle_panes": int(idle),
        "active_panes": 1,
        "measure_rc": int(rc),
        "window_on_screen_before_after": onscreen,
        "front_app_before_after": front,
        "font": font(run),
        "active_pane": {"pid": int(active), "size": before.get(active), "size_after": after.get(active)},
        "pane_sizes": [dict(pid=int(k), **v) for k, v in sorted(before.items())],
        "pane_sizes_after": [dict(pid=int(k), **v) for k, v in sorted(after.items())],
        "terminal_reported_sizes": open(grid_path).read().split() if os.path.exists(grid_path) else None,
        "stream": stream,
        "layers": layers,
        "runner": socket.gethostname().split(".")[0],
        "macos": platform.mac_ver()[0],
        "commit": os.environ.get("E2E_COMMIT"),
        "measured_at": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    })
    with open(out, "w") as f:
        json.dump(doc, f, indent=1)
        f.write("\n")
    parts = "  ".join(f"{k} x{v['processes']} {v['cpu_percent_of_one_core']}%" for k, v in sorted(layers.items()))
    print(f"    {term} {idle}+1: CPU {doc['cpu_percent_of_one_core']}% of one core over {w}s  ({parts})")
    a = doc["active_pane"]
    print(f"    active pane {a['size']} (after {a['size_after']}), window {onscreen}, font {doc['font']}")
    if stream:
        print(f"    stream: {stream['bytes_in_window']} bytes in the window, {stream['bytes']} in all over"
              f" {stream['ticks']} ticks, {stream['late_ticks']} late, max lag {stream['max_lag_ms']} ms;"
              f" load1 peak {doc['host_load1_peak']}")
    print(f"    {out}")
    if a["size_after"] is None:
        print("    the active pane's size after the window could not be read from its tty")
    elif a["size"] != a["size_after"]:
        print(f"FAIL: the active pane changed size during the window: {a['size']} -> {a['size_after']}")
        sys.exit(2)


def same_work(m, i, slack):
    """why the two readings of one N are not of the same work, or None"""
    bm, bi = (m.get("stream") or {}).get("bytes_in_window"), (i.get("stream") or {}).get("bytes_in_window")
    if not bm or not bi:
        return "no stream bytes inside a window"
    if abs(bm - bi) > BYTES_TOLERANCE * max(bm, bi):
        return f"stream bytes inside the window differ by more than 2%: marspot {bm}, iTerm2 {bi}"
    sm, si, fm, fi = m["active_pane"]["size"], i["active_pane"]["size"], m["font"], i["font"]
    if not fm["cell_w"] or not fi["cell_w"]:
        return f"a font's cell is unknown: marspot {fm}, iTerm2 {fi}"
    want_cols = sm["cols"] * fm["cell_w"] / fi["cell_w"]
    want_rows = sm["rows"] * fm["cell_h"] / fi["cell_h"]
    if abs(si["cols"] - want_cols) > slack or abs(si["rows"] - want_rows) > slack:
        return (f"active panes differ by more than the fonts force (+{slack} cells): marspot {sm} in"
                f" {fm['name']} {fm['point']}, iTerm2 {si} in {fi['name']} {fi['point']}, which would be"
                f" {want_cols:.1f} x {want_rows:.1f}")
    return None


def summary(results, idles):
    rows, bad = [], []
    slack = float(os.environ.get("ACTIVE_SIZE_SLACK", "3"))
    for n in idles.split():
        row, full = {"idle_panes": int(n)}, {}
        for term, app in APPS.items():
            p = os.path.join(results, f"active-{app}-{n}.json")
            if not os.path.exists(p):
                continue
            d = full[term] = read(p)
            row[term] = {k: d.get(k) for k in ("cpu_percent_of_one_core", "rss_kib_total", "processes", "layers",
                                               "host_load1_peak", "host_load1_series", "measure_rc", "terminal_version",
                                               "measured_at", "window_secs", "pattern_version", "font", "active_pane",
                                               "window_on_screen_before_after")}
            row[term]["stream_bytes"] = (d.get("stream") or {}).get("bytes")
            row[term]["stream_bytes_in_window"] = (d.get("stream") or {}).get("bytes_in_window")
        m, i = row.get("marspot"), row.get("iterm2")
        if "marspot" in full and "iterm2" in full:
            why = same_work(full["marspot"], full["iterm2"], slack)
            row["same_work"] = why is None
            if why:
                row["not_same_work"] = why
                bad.append(f"N={n}: {why}")
        if m and i and i["cpu_percent_of_one_core"]:
            row["marspot_over_iterm2"] = round(m["cpu_percent_of_one_core"] / i["cpu_percent_of_one_core"], 3)
        rows.append(row)
    doc = {"runner": socket.gethostname().split(".")[0], "macos": platform.mac_ver()[0],
           "commit": os.environ.get("E2E_COMMIT"),
           "iterm2_version": next((r["iterm2"]["terminal_version"] for r in rows if "iterm2" in r), None),
           "measured_on": datetime.date.today().isoformat(), "readings": rows}
    out = os.path.join(results, "active-vs-iterm.json")
    with open(out, "w") as f:
        json.dump(doc, f, indent=1)
        f.write("\n")
    print(f"==> one active + N idle on {doc['runner']}, {doc['measured_on']}, iTerm2 {doc['iterm2_version']}")
    for r in rows:
        m, i = r.get("marspot", {}), r.get("iterm2", {})
        print(f"    N={r['idle_panes']:<3} marspot {m.get('cpu_percent_of_one_core', '—')}%"
              f"  iTerm2 {i.get('cpu_percent_of_one_core', '—')}%  ratio {r.get('marspot_over_iterm2', '—')}"
              f"  bytes in window {m.get('stream_bytes_in_window', '—')} / {i.get('stream_bytes_in_window', '—')}")
    print(f"    {out}")
    for b in bad:
        print(f"FAIL: {b}")
    return 2 if bad else 0


if __name__ == "__main__":
    if sys.argv[1] == "one":
        one(*sys.argv[2:10])
    elif sys.argv[1] == "font":
        sys.exit(font_of(sys.argv[2], sys.argv[3]))
    else:
        sys.exit(summary(sys.argv[2], sys.argv[3]))
