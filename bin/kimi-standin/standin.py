#!/usr/bin/env python3
"""A stand-in for the Kimi Code CLI, for the suites that must not run
the real one on a runner.  Started by the compiled `kimi`
(bin/codex-standin/launcher.c built with this file), which stays the
process a pane shows: the kernel's name for it is `kimi`.

  kimi [--yolo] [-m alias]           the TUI, on a new session
  kimi --session <id> | -r <id>      the TUI on that session
  kimi -c | --continue               the TUI on the newest session here
  kimi doctor                        checks config.toml and mcp.json
  kimi --version                     the recorded version

The model is the one KIMI_MODEL_* names, as for the real CLI run
without an account.  What it draws and writes comes from exchange.json,
one recorded exchange with the real CLI (bin/record-kimi-exchange.sh).
"""
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
# nothing written next to the sources, which are a checkout
sys.dont_write_bytecode = True


def parse(argv, version):
    o = {"yolo": False, "model": None, "resume": None, "version": version}
    it = iter(argv)
    for a in it:
        if a in ("--yolo", "-y", "--yes"):
            o["yolo"] = True
        elif a in ("-m", "--model"):
            o["model"] = next(it, None)
        elif a in ("--session", "-S", "-r", "--resume"):
            o["resume"] = next(it, "")
        elif a in ("-c", "--continue"):
            o["resume"] = ""
    return o


def doctor(fixture):
    import peers
    home = os.environ.get("KIMI_CODE_HOME") or os.path.expanduser("~/.kimi-code")
    lines, bad = ["Kimi doctor", ""], False
    try:
        peers.hooks(home)
        lines.append("OK config.toml  %s" % os.path.join(home, "config.toml"))
    except (ValueError, IndexError) as e:
        lines.append("ERROR config.toml  %s: %s" % (os.path.join(home, "config.toml"), e))
        bad = True
    path = os.path.join(home, "mcp.json")
    if os.path.exists(path):
        try:
            servers = json.load(open(path)).get("mcpServers", {})
            if not all(isinstance(s, dict) and isinstance(s.get("command"), str) for s in servers.values()):
                raise ValueError("a server without a command")
        except (ValueError, AttributeError) as e:
            lines.append("ERROR mcp.json  %s: %s" % (path, e))
            bad = True
    if os.path.exists(os.path.join(home, "tui.toml")):
        lines.append("OK tui.toml     %s" % os.path.join(home, "tui.toml"))
    lines += ["", "Some config files are invalid." if bad else "All checked config files are valid."]
    print("\n".join(lines))
    return 1 if bad else 0


def main(argv):
    fixture = json.load(open(os.path.join(HERE, "exchange.json")))
    version = os.environ.get("KIMI_STANDIN_VERSION", fixture["version"])
    if argv[:1] in (["--version"], ["-V"]):
        print(version)
        return 0
    if argv[:1] == ["doctor"]:
        return doctor(fixture)
    import tui
    return tui.App(parse(argv, version), fixture).run()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
