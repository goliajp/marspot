"""What the stand-in TUI draws, laid out and styled as the recorded
Codex 0.171.0 screen: the header, the transcript, the working line, the
input box with its `›`, the footer, the window title and the 7501 state
report.  The bytes that set the terminal up and put it back, and the
goodbye, are the recorded ones."""
import os
import time

USER_BG = "\x1b[48;2;68;68;68m"
TITLE_WIDTH = 29  # how much of the thread id the recorded title shows


def _pick(fixture, segment, needle):
    for s in fixture.get("segments", []):
        if s["name"] == segment:
            for _, out in s["out"]:
                if needle in out:
                    return out
    return ""


def _no_kitty(s):
    # the progressive keyboard mode is left alone: the keys a test types
    # arrive as plain bytes
    for k in ("\x1b[>7u", "\x1b[<1u", "\x1b[<u"):
        s = s.replace(k, "")
    return s


def wrap(text, width):
    """Words onto lines no wider than `width`; a word wider is cut."""
    lines = []
    for para in text.split("\n"):
        line = ""
        for word in para.split(" "):
            while len(word) > width:
                if line:
                    lines.append(line)
                    line = ""
                lines.append(word[:width])
                word = word[width:]
            if not line:
                line = word
            elif len(line) + 1 + len(word) <= width:
                line += " " + word
            else:
                lines.append(line)
                line = word
        lines.append(line)
    return lines


class Screen:
    def __init__(self, fixture, version, cwd, model, bypass):
        self.f = fixture
        self.version = version
        self.cwd = cwd
        self.model = model or ""
        self.bypass = bypass
        self.entries = []
        self.working = False
        self.draft = ""
        self.title = None
        self.name = None
        self.thread = None

    def setup(self):
        out = ""
        for s in self.f.get("segments", []):
            if s["name"] == "start":
                for _, chunk in s["out"]:
                    out += chunk
                    if "\x1b[?1006h" in chunk:
                        return _no_kitty(out)
        return out

    def teardown(self, goodbye):
        out = _no_kitty(_pick(self.f, "quit", "\x1b[?1049l")) + "\x1b[0 q\x1b[?25h\x1b]7501;state=clear\x1b\\"
        if goodbye and self.thread:
            g = _pick(self.f, "quit", "Disconnected")
            out += g.replace("{THREAD}", self.thread)
        return out

    def state(self, word):
        return "\x1b]7501;state=%s:app=codex\x1b\\" % word

    def title_bytes(self):
        parts = []
        if self.name:
            parts.append(self.name)
        parts.append(os.path.basename(self.cwd.rstrip("/")) or "/")
        if self.thread:
            parts.append(self.thread[:TITLE_WIDTH] + "...")
        t = ("⠼ " if self.working else "") + " | ".join(parts)
        if t == self.title:
            return ""
        self.title = t
        return "\x1b]0;%s\x07" % t

    def add(self, kind, text):
        self.entries.append((kind, text))

    def _transcript(self, cols):
        lines = []
        w = max(10, cols - 2)
        for kind, text in self.entries:
            if kind == "user":
                blank = USER_BG + " " * cols
                lines.append(blank)
                for i, l in enumerate(wrap(text, w)):
                    lead = "\x1b[1m\x1b[2m› \x1b[22m" if i == 0 else "  "
                    lines.append(USER_BG + lead + USER_BG + l + " " * max(0, cols - 2 - len(l)))
                lines.append(blank)
            elif kind == "worked":
                lines.append("\x1b[2m  %s\x1b[0m" % text)
            else:
                mark = "\x1b[31m■\x1b[39m " if kind == "error" else "\x1b[2m•\x1b[22m "
                for i, l in enumerate(wrap(text, w)):
                    lines.append((mark if i == 0 else "  ") + l)
            lines.append("")
        return lines

    def frame(self):
        cols, rows = os.get_terminal_size(0)
        out = ["\x1b[?2026h\x1b[?25l\x1b[0m\x1b[H\x1b[J"]

        def at(r, s):
            if 1 <= r <= rows:
                out.append("\x1b[%d;1H%s\x1b[0m" % (r, s))

        at(2, "  \x1b[38;2;99;168;248;49m>_ \x1b[1m\x1b[39;49mOpenAI Codex\x1b[22m\x1b[2m (v%s)" % self.version)
        at(3, "     \x1b[2m%s" % self.cwd)
        if self.bypass:
            at(4, "\x1b[2m  permissions: \x1b[22m\x1b[1m\x1b[38;5;5;49mYOLO mode")
        top, bottom = 6, rows - 7
        lines = self._transcript(cols)
        if bottom >= top:
            shown = lines[-(bottom - top + 1):]
            for i, l in enumerate(shown):
                at(top + i, l)
        if self.working:
            at(rows - 6, "\x1b[1m\x1b[38;2;128;128;128;49m•\x1b[22m \x1b[2m\x1b[39;49mWorking (0s • \x1b[22m\x1b[1mesc\x1b[22m\x1b[2m to interrupt)")
        box = self.draft if self.draft else "\x1b[2mAsk Codex to do anything"
        at(rows - 3, "\x1b[1m›\x1b[22m " + box)
        at(rows - 1, "  \x1b[38;2;246;226;183;49m%s default\x1b[39;49m · \x1b[38;2;171;223;167;49m%s" % (self.model, self.cwd))
        at(rows, "  \x1b[1m←\x1b[22m for agents · \x1b[1m?\x1b[22m for shortcuts")
        out.append("\x1b[%d;%dH\x1b[?25h\x1b[?2026l" % (max(1, rows - 3), 3 + len(self.draft)))
        return "".join(out) + self.title_bytes()


def worked_line():
    return "Worked for <1s • " + time.strftime("%H:%M")
