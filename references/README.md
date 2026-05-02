# References

Local-only reference materials — open-source projects and specs we **read for reference** but do not depend on. The directory is gitignored except this manifest; each developer / AI session populates it from the URLs and commits below.

We consult these when the self-built implementation needs guidance on edge cases (e.g., obscure VT escape sequences, font fallback behavior, PTY edge conditions). They are studied, not vendored or imported.

## Manifest

Each entry: target subdir, upstream URL, pinned commit/tag, why it's here.

| Subdir | Upstream | Pin | Used for |
|---|---|---|---|
| `alacritty/` | https://github.com/alacritty/alacritty | tbd | Best-in-class Rust VT100/xterm emulator (`alacritty_terminal/`); also study its Metal/OpenGL renderer and PTY handling on macOS |
| `libvterm/` | https://github.com/neovim/libvterm | tbd | Compact C VT emulator (~7k LOC), embedded in Vim/Neovim. Easy to read for "what does this sequence do" questions |
| `ctlseqs/` | https://invisible-island.net/xterm/ctlseqs/ctlseqs.txt | n/a (snapshot) | Tom Dickey's canonical xterm control sequences reference — the de facto VT/xterm spec everything targets |

Add more as needed — record them here so future sessions know what's available.

## Populating

```sh
# from mars/ root
mkdir -p references && cd references

git clone --depth 1 https://github.com/alacritty/alacritty.git
git clone --depth 1 https://github.com/neovim/libvterm.git
mkdir -p ctlseqs && curl -L -o ctlseqs/ctlseqs.txt https://invisible-island.net/xterm/ctlseqs/ctlseqs.txt
```

When a reference becomes load-bearing for a particular self-built module (e.g., we cite a specific sequence interpretation), record the commit hash here so future verification is reproducible.
