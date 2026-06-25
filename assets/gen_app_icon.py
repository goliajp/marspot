#!/usr/bin/env python3
"""
marspot App Icon generator — horizontal hourglass (bowtie).

Renders at all standard macOS iconset sizes (16..1024 @ 1x/2x), bakes
into assets/Marspot.iconset/, then iconutil → assets/AppIcon.icns.

Geometry:
  - macOS icon canvas keeps a ~10 % safe inset so the artwork doesn't
    crowd the rounded mask;  squircle is drawn at full canvas.
  - Bowtie = two triangles meeting at center, lying on their sides:
      L = (sx, sy), (sx, sy + h), (cx, cy)
      R = (sx + w, sy), (sx + w, sy + h), (cx, cy)
    where (sx, sy, w, h) is the safe inset region and (cx, cy) is the
    center.
  - Filled with a vertical gradient (warm orange → red) on the squircle
    BG, white bowtie on top — keeps the silhouette readable at 16 × 16
    Dock thumbnail without being a fashion design.

Re-run after editing any constant below;  iconutil packs the iconset
into the .icns and install-local.sh deploys it on the next install.
"""

from PIL import Image, ImageDraw
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).parent
ICONSET = ROOT / "Marspot.iconset"
OUT_ICNS = ROOT / "AppIcon.icns"

# 10 macOS standard sizes; iconutil insists on these filenames.
SIZES = [
    ("icon_16x16.png",      16),
    ("icon_16x16@2x.png",   32),
    ("icon_32x32.png",      32),
    ("icon_32x32@2x.png",   64),
    ("icon_128x128.png",    128),
    ("icon_128x128@2x.png", 256),
    ("icon_256x256.png",    256),
    ("icon_256x256@2x.png", 512),
    ("icon_512x512.png",    512),
    ("icon_512x512@2x.png", 1024),
]

# Palette — warm sunset glass.
BG_TOP    = (251, 142,  60)   # warm orange
BG_BOTTOM = (220,  53,  53)   # deep red
GLASS     = (255, 255, 255)   # bowtie fill
FRAME     = (255, 255, 255)   # outline & stem (currently same)


def squircle_mask(size: int) -> Image.Image:
    """macOS squircle mask — superellipse n ≈ 5 at iOS / macOS radius."""
    mask = Image.new("L", (size, size), 0)
    px = mask.load()
    # Superellipse: |x|^n + |y|^n = r^n.  n = 5 reads close to Apple's
    # squircle.  Radius = size / 2 ; center = (size-1)/2.
    n = 5.0
    r = size / 2.0
    cx = cy = (size - 1) / 2.0
    for y in range(size):
        for x in range(size):
            nx = abs((x - cx) / r)
            ny = abs((y - cy) / r)
            if nx ** n + ny ** n <= 1.0:
                px[x, y] = 255
    return mask


def vertical_gradient(size: int, top, bottom) -> Image.Image:
    img = Image.new("RGB", (size, size), bottom)
    px = img.load()
    for y in range(size):
        t = y / max(1, size - 1)
        r = round(top[0] * (1 - t) + bottom[0] * t)
        g = round(top[1] * (1 - t) + bottom[1] * t)
        b = round(top[2] * (1 - t) + bottom[2] * t)
        for x in range(size):
            px[x, y] = (r, g, b)
    return img


def draw_bowtie(img: Image.Image, size: int) -> None:
    """Horizontal hourglass — two triangles meeting at the center.

    Geometry derived from a 10 % safe inset so the bowtie doesn't
    visually crowd the squircle mask at the corners.
    """
    inset = size * 0.16    # bowtie inset from squircle edge
    sx = inset
    sy = inset
    w = size - 2 * inset
    h = size - 2 * inset
    cx = size / 2.0
    cy = size / 2.0

    draw = ImageDraw.Draw(img)
    # Left triangle: pointing right toward center.
    draw.polygon(
        [(sx, sy), (sx, sy + h), (cx, cy)],
        fill=GLASS,
    )
    # Right triangle: pointing left toward center.
    draw.polygon(
        [(sx + w, sy), (sx + w, sy + h), (cx, cy)],
        fill=GLASS,
    )


def render(size: int) -> Image.Image:
    """Compose one icon: gradient squircle BG + white bowtie on top."""
    bg = vertical_gradient(size, BG_TOP, BG_BOTTOM)
    bg = bg.convert("RGBA")
    # Mask the gradient to the squircle.
    mask = squircle_mask(size)
    bg.putalpha(mask)
    # Draw bowtie on top.
    draw_bowtie(bg, size)
    return bg


def main() -> int:
    ICONSET.mkdir(parents=True, exist_ok=True)
    for name, px in SIZES:
        out = ICONSET / name
        render(px).save(out, "PNG", optimize=True)
        print(f"  wrote {out.relative_to(ROOT.parent)}  ({px}x{px})")
    # iconutil packs the iconset into .icns.
    rc = subprocess.run(
        ["iconutil", "--convert", "icns", str(ICONSET), "-o", str(OUT_ICNS)],
        check=False,
    ).returncode
    if rc != 0:
        print(f"iconutil failed (exit {rc})", file=sys.stderr)
        return rc
    print(f"\n→ {OUT_ICNS.relative_to(ROOT.parent)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
