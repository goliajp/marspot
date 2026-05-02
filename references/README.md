# References

Local-only reference materials — open-source projects and specs we **may consult when stuck**, but never copy from. Gitignored except this manifest.

## Discipline (read this first)

**Default mode is "references closed."** When implementing a module — emulator, renderer, scrollback, anything — write from first principles. Do not preemptively browse references; doing so anchors our design to existing solutions and locks us into their ceiling.

**Open a reference only when blocked on a specific question.** Examples of legitimate triggers:
- "What's the right interpretation of this CSI sequence parameter when omitted?"
- "How does Alacritty avoid GPU stalls when scrolling?"
- "What's the right way to coalesce TIOCSWINSZ during a live resize?"

When you do consult one:
- Read for **insight** (the *idea*, the *invariant*, the *failure mode they hit*) — not for code to translate
- Distill the insight into a one-line note in the relevant code comment or design doc
- **Do not** copy structure, naming, file layout, type signatures, or comments verbatim
- Mars's goal is to be **more advanced** than these references — copying ceilings them

If you find yourself reading a reference to "see how to do X" before trying X yourself, stop and try X first. The constraint is what produces a better design.

## Manifest

Each entry: target subdir, upstream URL, pinned commit/tag, why it's available.

| Subdir | Upstream | Pin | Available for (when stuck on…) |
|---|---|---|---|
| `alacritty/` | https://github.com/alacritty/alacritty | tbd | VT100/xterm edge cases, GPU renderer ideas, PTY handling on macOS |
| `libvterm/` | https://github.com/neovim/libvterm | tbd | "What does this VT sequence actually do" — compact C reference (~7k LOC) |
| `ctlseqs/` | https://invisible-island.net/xterm/ctlseqs/ctlseqs.txt | n/a (snapshot) | The canonical xterm control sequences spec — Tom Dickey's exhaustive reference |

Add more references as needed; record what they're for so future sessions know.

## Populating

```sh
# from mars/ root
mkdir -p references && cd references

git clone --depth 1 https://github.com/alacritty/alacritty.git
git clone --depth 1 https://github.com/neovim/libvterm.git
mkdir -p ctlseqs && curl -L -o ctlseqs/ctlseqs.txt https://invisible-island.net/xterm/ctlseqs/ctlseqs.txt
```

When a reference becomes load-bearing for a specific design choice (we cite a particular interpretation), record the commit hash here so the citation is reproducible.
