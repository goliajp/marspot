"""What the stand-in TUI draws, laid out and styled as the recorded
Kimi Code 2.7.3 screen: the welcome box, the transcript lines (the
prompt, the reply, a tool call, an error), and at the bottom the live
part that is drawn again in place: a status line, the input box with
its `│ > ` and the footer.  The bytes that set the terminal up are the
recorded ones."""
import time
import unicodedata

BLUE = "\x1b[38;2;79;168;255m"
RULE = "\x1b[38;2;90;90;90m"
DIM = "\x1b[38;2;136;136;136m"
YELLOW = "\x1b[38;2;255;203;107m"
WHITE = "\x1b[38;2;224;224;224m"
GREEN = "\x1b[38;2;78;200;126m"
RED = "\x1b[38;2;232;84;84m"
ORANGE = "\x1b[38;2;232;168;56m"
HINT = "\x1b[38;2;107;107;107m"
BOLD, NOBOLD, FG, RESET = "\x1b[1m", "\x1b[22m", "\x1b[39m", "\x1b[0m"
END_SYNC, BEGIN_SYNC = "\x1b[?2026l", "\x1b[?2026h"
# the progressive keyboard mode is left out: keys arrive as plain bytes
KITTY = ("\x1b[>7u",)
TEARDOWN = "\x1b[?2031l\x1b[?1004l\x1b[<u\x1b[>4;0m"


def setup(fixture):
    """The recorded bytes kimi sends before its first frame: the modes
    it sets and the queries it asks the terminal."""
    first = "".join(out for _, out in fixture["segments"][0]["out"])
    head = first[:first.index("\x1b[>4;2m") + len("\x1b[>4;2m")]
    for k in KITTY:
        head = head.replace(k, "")
    return head


def width(s):
    return sum(2 if unicodedata.east_asian_width(c) in "WF" else 0 if unicodedata.combining(c) else 1 for c in s)


def wrap(text, w):
    lines = []
    for para in text.split("\n"):
        line = ""
        for ch in para:
            if width(line + ch) > w:
                lines.append(line)
                line = ""
            line += ch
        lines.append(line)
    return lines


def pad(s, w):
    return s + " " * max(0, w - width(s))


def tail_of(cwd):
    parts = cwd.rstrip("/").split("/")
    return "…/" + "/".join(parts[-3:]) if len(parts) > 4 else cwd


def welcome(cols, cwd, session, model, version, servers):
    """The box kimi opens with, and the lines under it; `servers` is
    (name, tool count) for every MCP server that answered."""
    inner = cols - 4
    side, close = " " + BLUE + "│" + FG, BLUE + "│" + FG
    out = [" " + BLUE + "╭" + "─" * inner + "╮" + FG, side + " " * inner + close]
    for logo, styled, plain in (("▐█▛█▛█▌", BOLD + BLUE + "Welcome to Kimi Code!" + FG + NOBOLD, "Welcome to Kimi Code!"),
                                ("▐█████▌", DIM + "Send /help for help information." + FG, "Send /help for help information.")):
        out.append(side + "  " + BLUE + logo + FG + "  " + styled + " " * (inner - 11 - width(plain)) + close)
    out.append(side + " " * inner + close)
    fields = [("Directory: ", cwd), ("Session:   ", session), ("Model:     ", model), ("Version:   ", version)]
    if servers:
        fields.append(("MCP:       ", "%d connected" % len(servers)))
    for label, value in fields:
        value = value[:inner - 13]
        out.append(side + "  " + BOLD + DIM + label + FG + NOBOLD + pad(value, inner - 13) + close)
    out += [side + " " * inner + close, " " + BLUE + "╰" + "─" * inner + "╯" + FG, " "]
    if not session:
        out.append("   " + DIM + "No session yet — one will be created on your first message." + FG)
    for name, n in servers:
        out.append("   " + GREEN + 'MCP server "%s" connected · %d tool%s (stdio)' % (name, n, "" if n == 1 else "s") + FG)
    return out


def user(cols, text):
    lines = wrap(text, cols - 6)
    out = ["\x1b]133;A\x07 ", "\x1b]133;B\x07\x1b]133;C\x07 " + YELLOW + BOLD + "❯ " + lines[0] + NOBOLD + FG]
    out += ["   " + YELLOW + BOLD + line + NOBOLD + FG for line in lines[1:]]
    return out + [" "]


def reply(cols, text):
    lines = wrap(text or "", cols - 6)
    return [" " + WHITE + "● " + FG + lines[0]] + ["   " + line for line in lines[1:]] + [" "]


def tool(cols, server, name, to, out):
    head = " " + GREEN + "● " + FG + "Used " + BLUE + BOLD + name + NOBOLD + FG + "\x1b[2m · MCP/%s (%s)\x1b[22m" % (server, to)
    return [head] + ["   \x1b[2m" + line + "\x1b[22m" for line in wrap(out, cols - 6)[:3]] + [" "]


def error(cols, err):
    first = "Error: [%s] %s" % (err["code"], err["message"])
    return ["   " + RED + line + FG for line in wrap(first, cols - 4)] + [
        "   " + DIM + "If this persists, run `/export-debug-zip` and share the file with us for diagnosis."[:cols - 4] + FG, " "]


class Live:
    """The bottom part, drawn again in place; `up` is how far the cursor
    sits below its first line."""

    def __init__(self):
        self.up = None

    def frame(self, cols, status, typed, model, cwd, above=()):
        w = cols - 7
        lines = wrap(typed, w - 1) if typed else [""]
        box = [" " + RULE + "╭" + "─" * (cols - 4) + "╮" + FG]
        for i, line in enumerate(lines):
            lead = " > " if i == 0 else "   "
            cursor = "\x1b[7m \x1b[27m" if i == len(lines) - 1 else " "
            box.append(" " + RULE + "│" + FG + lead + line + cursor + " " * max(0, w - 1 - width(line)) + RULE + "│" + FG)
        box.append(" " + RULE + "╰" + "─" * (cols - 4) + "╯" + FG)
        left = "Ask When Needed  %s thinking  %s" % (model, tail_of(cwd))
        foot = (" " + ORANGE + BOLD + "Ask When Needed" + NOBOLD + FG + "  " + WHITE + model + " thinking" + FG + "  " +
                DIM + tail_of(cwd) + FG)
        hint = "@: mention files | ! to run a shell command"
        if width(left) + 4 + len(hint) < cols:
            foot += " " * (cols - 2 - width(left) - len(hint)) + HINT + hint + FG
        ctx = "context: 0% (0/256k)"
        region = [status] + box + [foot, " " * (cols - 2 - len(ctx)) + WHITE + ctx + FG]
        at = 1 + len(lines)
        out = BEGIN_SYNC
        if self.up is not None:
            out += ("\x1b[%dA" % self.up if self.up else "") + "\r"
        out += "\x1b[J"
        for line in above:
            out += line + RESET + "\r\n"
        out += (RESET + "\r\n").join(region) + RESET
        back = len(region) - 1 - at
        out += ("\x1b[%dA" % back if back else "") + "\x1b[%dG" % (6 + width(lines[-1])) + END_SYNC
        self.up = at
        self.below = back
        return out


def status_idle():
    return " "


def status_busy():
    return "  🌑" + DIM + " · Tip: /sessions to browse and resume earlier sessions" + FG


def status_done(ms):
    secs = max(1, round(ms / 1000))
    return "  " + DIM + "Worked for %ds · done %s" % (secs, time.strftime("%I:%M %p").lstrip("0")) + FG


def goodbye(below, session):
    out = TEARDOWN + ("\x1b[%dB" % below if below else "") + "\r\r\n\x1b[?25h\x1b[?2004l Bye!\r\n\r\n"
    if session:
        out += " To resume this session: kimi -r %s\r\n" % session
    return out

