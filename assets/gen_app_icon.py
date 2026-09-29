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

from PIL import Image, ImageDraw, ImageFilter
from pathlib import Path
import math
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

# Palette — 黑底白 bowtie,跟 macOS terminal app icon 风格一致.
# 不纯黑(#000)而是 #181818,Dock 上有微妙渐变才有质感.
BG_TOP    = ( 32,  32,  32)
BG_BOTTOM = ( 12,  12,  12)
GLASS     = (255, 255, 255)
FRAME     = (255, 255, 255)


# Apple HIG App Icon Grid(2024):master 1024×1024,visible body 824×824
# 居中(100px transparent margin 每边 = 0.0977 ratio = ~80.5%).
# 严格 spec 视觉偏小,因 system apps(Safari/Mail/Finder)靠 drop
# shadow + 色彩饱和度撑场面 —— 这里走折中:body 上调到 ~86%,加
# 软阴影补齐余下视觉差距.
CANVAS_INSET_RATIO: float = 0.083  # body ~83.4 % canvas — 介乎 spec 80.5% 跟 86% 之间
# Drop shadow:offset (0, ~1 % canvas) + blur ~2 % canvas.Apple 系统
# icon shadow 大约这个量级,凸显但不夺主.
SHADOW_OFFSET_RATIO: float = 0.01
SHADOW_BLUR_RATIO:   float = 0.022
SHADOW_ALPHA:        int   = 110


def squircle_mask(size: int) -> Image.Image:
    """macOS squircle mask — superellipse n ≈ 5 at iOS / macOS radius.

    Drawn at ~80 % of canvas with transparent border around for Dock
    parity with native apps (Safari / Finder / Calendar etc.).
    """
    mask = Image.new("L", (size, size), 0)
    px = mask.load()
    inset = size * CANVAS_INSET_RATIO
    sq_size = size - 2 * inset
    n = 5.0
    r = sq_size / 2.0
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


def _rounded_polygon(vertices, corner_radii, n_arc=14):
    """Replace each vertex(parallel `corner_radii` 控制 per-vertex 半径,
    0 = 尖角)with a quadratic-bezier fillet:沿入/出两边各退 r 距离
    取两端点 A、B,以原 vertex 作 control point 弯弧.小半径下贴近
    真圆角,够 dev tool / 小尺寸 icon 用.
    """
    n = len(vertices)
    out: list[tuple[float, float]] = []
    for i in range(n):
        v = vertices[i]
        r = corner_radii[i]
        if r <= 0:
            out.append(v)
            continue
        prev_v = vertices[(i - 1) % n]
        next_v = vertices[(i + 1) % n]

        def unit(p, q):
            dx, dy = q[0] - p[0], q[1] - p[1]
            l = math.hypot(dx, dy) or 1.0
            return (dx / l, dy / l)

        u_in = unit(v, prev_v)
        u_out = unit(v, next_v)
        a = (v[0] + u_in[0] * r, v[1] + u_in[1] * r)
        b = (v[0] + u_out[0] * r, v[1] + u_out[1] * r)
        for j in range(n_arc + 1):
            t = j / n_arc
            x = (1 - t) ** 2 * a[0] + 2 * (1 - t) * t * v[0] + t ** 2 * b[0]
            y = (1 - t) ** 2 * a[1] + 2 * (1 - t) * t * v[1] + t ** 2 * b[1]
            out.append((x, y))
    return out


def draw_bowtie(img: Image.Image, size: int) -> None:
    """Horizontal hourglass — 两片三角形 cap-to-cap,四个外角小圆角,
    中心相会点保持尖.圆角半径 BOWTIE_CORNER_RADIUS_RATIO 之于 size.
    """
    canvas_inset = size * CANVAS_INSET_RATIO
    bowtie_inset_inside_squircle = size * 0.21
    inset = canvas_inset + bowtie_inset_inside_squircle
    sx = inset
    sy = inset
    w = size - 2 * inset
    h = size - 2 * inset
    cx = size / 2.0
    cy = size / 2.0

    # 四个外角圆角半径 ≈ size * 0.015 — "有一丁点弧度",刚好够
    # 16×16 Dock 看得出,32+ 是显著但不抢眼.
    r_corner = size * 0.015

    draw = ImageDraw.Draw(img)
    # 左三角:外两顶点圆角,中心点尖.
    left = _rounded_polygon(
        [(sx, sy), (sx, sy + h), (cx, cy)],
        [r_corner, r_corner, 0.0],
    )
    draw.polygon(left, fill=GLASS)
    # 右三角:同样.
    right = _rounded_polygon(
        [(sx + w, sy), (cx, cy), (sx + w, sy + h)],
        [r_corner, 0.0, r_corner],
    )
    draw.polygon(right, fill=GLASS)


def render(size: int) -> Image.Image:
    """Compose one icon: drop shadow + gradient squircle + white bowtie."""
    # 1. Gradient squircle body.
    body = vertical_gradient(size, BG_TOP, BG_BOTTOM).convert("RGBA")
    body.putalpha(squircle_mask(size))
    # 2. Draw bowtie onto body so the shape composites under one mask.
    draw_bowtie(body, size)
    # 3. Shadow: black squircle mask blurred + offset, composited under body.
    shadow_mask = squircle_mask(size)
    shadow_layer = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    shadow_solid = Image.new("RGBA", (size, size), (0, 0, 0, SHADOW_ALPHA))
    shadow_layer.paste(shadow_solid, (0, 0), shadow_mask)
    blur_px = max(1, int(round(size * SHADOW_BLUR_RATIO)))
    shadow_layer = shadow_layer.filter(ImageFilter.GaussianBlur(radius=blur_px))
    offset_y = int(round(size * SHADOW_OFFSET_RATIO))
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    canvas.alpha_composite(shadow_layer, (0, offset_y))
    canvas.alpha_composite(body)
    return canvas


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
