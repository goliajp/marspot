#!/usr/bin/env python3
"""Where the time from a key press to the frame that shows it went.

    bin/_latency_report.py PREFIX [--json OUT] [--label TEXT] [--run 20]

Reads the two files a `latency-trace` build writes when
MARSPOT_LATENCY_TRACE=PREFIX (one from the shell, one from the core;
records are described in src/latency_trace.rs), follows every scripted
key through both, and prints each stage's p50 / p90 / p99 / max in
microseconds, with what every frame cost and how many were produced,
presented and shown.

Two checks that fail differently from the clock arithmetic:
  - the pixels: the script types runs of N `x` and then N backspaces, so
    the surface presented for the k-th backspace has to hash the same
    as the one presented for the (N-k)-th `x`.  A frame attributed to
    the wrong key breaks that.
  - the counts: keys sent, keys the core read, frames attributed.
"""
import argparse
import glob
import json
import struct
import sys

STAGES = {
    1: "KeyEventTime", 2: "KeyDown", 3: "KeySent", 4: "CoreKeyRead", 5: "CoreKeyHandled",
    6: "GridReady", 7: "PumpBegin", 8: "PumpEnd", 9: "RenderBegin", 10: "RenderCommitted",
    11: "Settled", 12: "SurfaceSent", 13: "ShellSurfaceRead", 14: "PresentBegin",
    15: "PresentEnd", 16: "OnScreen", 17: "ShownSent", 18: "FrameShown", 32: "BuildUs", 33: "PanesUs", 34: "EncodeUs",
    35: "GpuExecUs", 36: "UploadBytes", 37: "RowsRebuilt", 38: "RowsTotal",
    40: "DrawableUs", 41: "ScheduledUs", 42: "SurfaceHash",
}


def read(path):
    data = open(path, "rb").read()
    out = []
    for off in range(0, len(data) - 15, 16):
        stage, aux, t = struct.unpack_from("<B3xIQ", data, off)
        out.append((STAGES.get(stage, f"?{stage}"), aux, t))
    return out


def pct(values, p):
    if not values:
        return None
    v = sorted(values)
    return v[min(len(v) - 1, int(round(p / 100 * (len(v) - 1))))]


def summary(values):
    if not values:
        return None
    return {"n": len(values), "p50": pct(values, 50), "p90": pct(values, 90),
            "p99": pct(values, 99), "max": max(values), "mean": round(sum(values) / len(values), 1)}


def first_at_or_after(times, t, start):
    """Index of the first entry of the sorted list `times` that is >= t, from `start`."""
    i = start
    while i < len(times) and times[i] < t:
        i += 1
    return i if i < len(times) else None


BOOT_EVENTS = ("STARTUP", "CORE_SPAWN", "CORE_BOOT", "RENDERER_SHADERS", "PRESENTER_SHADERS",
               "SESSION_BOOT", "CORE_LOOP", "WINDOW_FIRST_FRAME", "WINDOW_FIRST_PRESENT")


def launch(log_path):
    """Milliseconds from the shell's STARTUP line to the first of each boot event."""
    first, fields = {}, {}
    try:
        lines = open(log_path, errors="replace").read().splitlines()
    except OSError:
        return None
    for line in lines:
        cols = line.split("\t")
        if len(cols) < 8 or cols[6] not in BOOT_EVENTS or cols[6] in first:
            continue
        first[cols[6]] = int(cols[1])
        fields[cols[6]] = dict(c.split("=", 1) for c in cols[8:] if "=" in c)
    if "STARTUP" not in first:
        return None
    out = {name: first[name] - first["STARTUP"] for name in BOOT_EVENTS if name in first}
    for name in ("RENDERER_SHADERS", "PRESENTER_SHADERS"):
        for key in ("library_us", "pipelines_us"):
            if key in fields.get(name, {}):
                out[f"{name}.{key}"] = int(fields[name][key])
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("prefix")
    ap.add_argument("--log", help="the sandbox's marspot.log, for the launch times")
    ap.add_argument("--json")
    ap.add_argument("--label", default="")
    ap.add_argument("--run", type=int, default=20, help="keys in a run of x, and of backspaces")
    args = ap.parse_args()

    shell_files = sorted(glob.glob(args.prefix + ".marspot-shell.*.bin"))
    core_files = sorted(glob.glob(args.prefix + ".marspot-core.*.bin"))
    if len(shell_files) != 1 or len(core_files) != 1:
        sys.exit(f"latency-report: want one shell and one core trace, found {shell_files} {core_files}")
    shell, core = read(shell_files[0]), read(core_files[0])
    if not shell or not core:
        sys.exit(f"latency-report: empty trace ({len(shell)} shell records, {len(core)} core records)")

    # the shell's presents, in file order, each with what was recorded about it
    presents, keys = [], []
    surface_reads = []
    for name, aux, t in shell:
        if name == "KeyEventTime":
            keys.append({"event": t})
        elif name == "KeyDown" and keys:
            keys[-1]["down"] = t
        elif name == "KeySent" and keys:
            keys[-1].setdefault("sent", t)
        elif name == "ShellSurfaceRead":
            surface_reads.append(t)
        elif name == "PresentBegin":
            presents.append({"begin": t})
        elif name == "OnScreen" and keys:
            keys[-1]["on_screen"], keys[-1]["capture_us"] = t, aux
        elif presents and name == "PresentEnd":
            presents[-1]["end"] = t
        elif presents and name in ("DrawableUs", "ScheduledUs", "SurfaceHash"):
            presents[-1][name] = aux
    presents = [p for p in presents if "end" in p]

    # the core's frames
    frames, cur = [], None
    key_reads, key_handled, grid_ready, pumps = [], [], [], []
    settled_frames = []
    for name, aux, t in core:
        if name == "CoreKeyRead":
            key_reads.append(t)
        elif name == "CoreKeyHandled":
            key_handled.append(t)
        elif name == "GridReady":
            grid_ready.append(t)
        elif name == "PumpBegin":
            pumps.append({"begin": t})
        elif name == "PumpEnd" and pumps:
            pumps[-1]["end"] = t
        elif name == "RenderBegin":
            cur = {"begin": t}
            frames.append(cur)
        elif cur is not None and name == "RenderCommitted":
            cur["committed"] = t
        elif cur is not None and name in ("BuildUs", "PanesUs", "EncodeUs", "UploadBytes", "RowsRebuilt", "RowsTotal"):
            cur[name] = aux
        elif name == "Settled":
            pending = [f for f in frames if "committed" in f and "settled" not in f]
            if pending:
                pending[-1]["settled"] = t
                settled_frames.append(pending[-1])
        elif name == "GpuExecUs" and settled_frames:
            settled_frames[-1]["GpuExecUs"] = aux
        elif name == "SurfaceSent" and settled_frames:
            settled_frames[-1].setdefault("sent", t)
    pumps = [p for p in pumps if "end" in p]
    key_reads.sort()
    grid_ready.sort()
    surface_reads.sort()

    stage_names = [
        ("event -> keyDown entered", "event", "down"),
        ("keyDown -> queued for the core", "down", "sent"),
        ("-> core's reader has it", "sent", "core_read"),
        ("-> core's loop passed it to the session", "core_read", "core_handled"),
        ("-> session says its grid changed", "core_handled", "grid_ready"),
        ("-> core's loop begins the pump", "grid_ready", "pump_begin"),
        ("pump (copy the grid in)", "pump_begin", "pump_end"),
        ("-> render begins (frame gate, frame in flight)", "pump_end", "render_begin"),
        ("build + encode + commit", "render_begin", "committed"),
        ("-> core sees the GPU finished", "committed", "settled"),
        ("-> announcement queued", "settled", "surface_sent"),
        ("-> shell's reader has it", "surface_sent", "shell_read"),
        ("-> shell's main thread begins present", "shell_read", "present_begin"),
        ("present (drawable, draw, schedule)", "present_begin", "present_end"),
        ("present returned -> a capture of the window shows it", "present_end", "on_screen"),
    ]
    chains = []
    ri = gi = pi = fi = si = qi = 0
    for n, k in enumerate(keys):
        if "down" not in k or "sent" not in k:
            continue
        c = dict(index=n, erase=(n // args.run) % 2 == 1, event=k["event"], down=k["down"], sent=k["sent"])
        ri = first_at_or_after(key_reads, k["down"], ri)
        if ri is None:
            break
        c["core_read"] = key_reads[ri]
        handled = [t for t in key_handled if t >= c["core_read"]]
        if not handled:
            break
        c["core_handled"] = handled[0]
        gi = first_at_or_after(grid_ready, c["core_handled"], gi)
        if gi is None:
            break
        c["grid_ready"] = grid_ready[gi]
        while pi < len(pumps) and pumps[pi]["end"] < c["grid_ready"]:
            pi += 1
        if pi >= len(pumps):
            break
        c["pump_begin"], c["pump_end"] = pumps[pi]["begin"], pumps[pi]["end"]
        while fi < len(frames) and frames[fi]["begin"] < c["pump_end"]:
            fi += 1
        if fi >= len(frames) or "settled" not in frames[fi] or "sent" not in frames[fi]:
            continue
        f = frames[fi]
        c.update(render_begin=f["begin"], committed=f["committed"], settled=f["settled"], surface_sent=f["sent"])
        si = first_at_or_after(surface_reads, f["sent"], si)
        if si is None:
            break
        c["shell_read"] = surface_reads[si]
        while qi < len(presents) and presents[qi]["begin"] < c["shell_read"]:
            qi += 1
        if qi >= len(presents):
            break
        p = presents[qi]
        c.update(present_begin=p["begin"], present_end=p["end"], hash=p.get("SurfaceHash"))
        if "on_screen" in k:
            c["on_screen"], c["capture_us"] = k["on_screen"], k["capture_us"]
        # a key whose frame was only presented after the next key was pressed was not followed
        nxt = keys[n + 1]["event"] if n + 1 < len(keys) else None
        c["overlapped"] = nxt is not None and c["present_end"] > nxt
        chains.append(c)

    def us(a, b, rows):
        return [(r[b] - r[a]) / 1000 for r in rows if a in r and b in r]

    clean = [c for c in chains if not c["overlapped"]]
    out = {"label": args.label, "keys_sent": len(keys), "keys_read_by_core": len(key_reads),
           "keys_followed_to_a_present": len(clean), "keys_overlapping_the_next": len(chains) - len(clean),
           "frames_rendered": len(frames), "frames_settled": len(settled_frames),
           "presents": len(presents)}
    out["keys_seen_in_a_capture"] = sum("on_screen" in c for c in clean)

    print(f"== {args.label or args.prefix}")
    print(f"   keys sent {out['keys_sent']}, read by the core {out['keys_read_by_core']}, "
          f"followed to a present {out['keys_followed_to_a_present']} "
          f"(dropped as overlapping the next key: {out['keys_overlapping_the_next']})")
    print(f"   frames rendered {out['frames_rendered']}, settled {out['frames_settled']}, presented {out['presents']}; "
          f"keys seen in a capture of the window {out['keys_seen_in_a_capture']}")

    for kind, rows in (("all keys", clean), ("x", [c for c in clean if not c["erase"]]),
                       ("backspace", [c for c in clean if c["erase"]])):
        if not rows:
            continue
        table = {}
        print(f"-- {kind}: {len(rows)} keys, microseconds          p50      p90      p99      max")
        for title, a, b in stage_names:
            s = summary(us(a, b, rows))
            if s:
                table[title] = s
                print(f"   {title:<48} {s['p50']:8.0f} {s['p90']:8.0f} {s['p99']:8.0f} {s['max']:8.0f}")
        for title, a, b in (("TOTAL event -> present returned", "event", "present_end"),
                            ("TOTAL event -> a capture of the window shows it", "event", "on_screen"),
                            ("of which inside the core (read -> announced)", "core_read", "surface_sent"),
                            ("of which the session (handled -> grid ready)", "core_handled", "grid_ready")):
            s = summary(us(a, b, rows))
            if s:
                table[title] = s
                print(f"   {title:<48} {s['p50']:8.0f} {s['p90']:8.0f} {s['p99']:8.0f} {s['max']:8.0f}")
        out[kind] = table

    per_frame = {}
    print("-- every frame the core rendered, microseconds     p50      p90      p99      max")
    for field in ("BuildUs", "PanesUs", "EncodeUs", "GpuExecUs", "UploadBytes", "RowsRebuilt", "RowsTotal"):
        s = summary([f[field] for f in frames if field in f])
        if s:
            per_frame[field] = s
            print(f"   {field:<48} {s['p50']:8.0f} {s['p90']:8.0f} {s['p99']:8.0f} {s['max']:8.0f}")
    for field in ("DrawableUs", "ScheduledUs"):
        s = summary([p[field] for p in presents if field in p])
        if s:
            per_frame[field] = s
            print(f"   shell {field:<42} {s['p50']:8.0f} {s['p90']:8.0f} {s['p99']:8.0f} {s['max']:8.0f}")
    out["per_frame"] = per_frame

    # pixels: the k-th backspace of a run shows what the (N-k)-th x showed
    by_index = {c["index"]: c for c in clean if c.get("hash") is not None}
    checked = matched = changed = pairs = 0
    for n, c in by_index.items():
        prev = by_index.get(n - 1)
        if prev is not None:
            pairs += 1
            changed += c["hash"] != prev["hash"]
        if not c["erase"]:
            continue
        k = n % args.run + 1  # this is the k-th backspace of its run
        if k == args.run:
            continue  # the empty line again; its twin is the frame before the run, another key's
        twin = by_index.get((n // args.run) * args.run - k - 1 + 0)
        # run of x covers indices base-N .. base-1; after k backspaces N-k remain: the x at base-k-1
        if twin is None:
            continue
        checked += 1
        matched += twin["hash"] == c["hash"]
    out["pixel_check"] = {"backspace_frames_checked": checked, "matching_their_x_frame": matched,
                          "consecutive_keys_compared": pairs, "whose_pixels_differ": changed}
    print(f"-- pixels: {matched} of {checked} backspace frames hash the same as the x frame with the same text; "
          f"{changed} of {pairs} consecutive keys presented different pixels")

    if args.log:
        out["launch_ms"] = launch(args.log)
        if out["launch_ms"]:
            print("-- launch, ms after the shell's STARTUP line: "
                  + ", ".join(f"{k} {v}" for k, v in out["launch_ms"].items()))

    if args.json:
        with open(args.json, "w") as fh:
            json.dump(out, fh, indent=1)
            fh.write("\n")
    captures = [c["capture_us"] for c in clean if "capture_us" in c]
    if captures:
        s = summary(captures)
        out["capture_us"] = s
        print(f"-- one capture took p50 {s['p50']} / p90 {s['p90']} / max {s['max']} us: the resolution of the capture rows")
    if keys and not clean:
        sys.exit("latency-report: no key could be followed to a present")


if __name__ == "__main__":
    main()
