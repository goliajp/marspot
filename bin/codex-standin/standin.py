#!/usr/bin/env python3
"""A stand-in for the Codex CLI, for the suites that must not run the
real one on a runner.  Started by the compiled `codex` (launcher.c),
which stays the process a pane shows: argv[0] `codex`, an executable
inside a release directory of the packaged layout.

  codex [-a ..] [-s ..] [-c k=v] [-m ..] [prompt]   the TUI, a new thread
  codex resume <id> | resume --last                  the TUI on a thread
  codex app-server [--listen unix://]                the app-server, in front
  codex --version                                    `codex-cli <release>`

The release is read off the launcher's own path
(`…/releases/<release>-<target>/bin/codex`), else CODEX_STANDIN_VERSION.
What it draws comes from exchange.json, one recorded exchange with the
real CLI (bin/record-codex-exchange.sh).
"""
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
# nothing written next to the sources, which are a checkout
sys.dont_write_bytecode = True


def version():
    parts = os.environ.get("CODEX_STANDIN_EXE", "").split("/")
    if "releases" in parts:
        i = len(parts) - 1 - parts[::-1].index("releases")
        if i + 1 < len(parts):
            return parts[i + 1].split("-")[0]
    return os.environ.get("CODEX_STANDIN_VERSION", "0.171.0")


def main(argv):
    fixture = os.path.join(HERE, "exchange.json")
    if argv[:1] == ["--daemon"]:
        import daemon
        daemon.main(argv[1], argv[2], fixture, argv[3] == "managed")
        return 0
    if argv[:1] in (["--version"], ["-V"]):
        print("codex-cli %s" % version())
        return 0
    if argv[:1] == ["app-server"]:
        import daemon
        home = os.environ.get("CODEX_HOME") or os.path.expanduser("~/.codex")
        daemon.main(home, version(), fixture, False)
        return 0
    import tui
    return tui.Tui(argv, version(), json.load(open(fixture))).run()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
