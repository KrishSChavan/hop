"""Draws Hop's app icon: two screens and a cursor hopping between them."""
import math
import os
import subprocess
import sys

from PIL import Image, ImageDraw

OUT = sys.argv[1]
SS = 4  # supersampling factor


def lerp(a, b, t):
    return tuple(int(round(a[i] + (b[i] - a[i]) * t)) for i in range(len(a)))


def icon(size, inset):
    """One icon `size` pixels square; the tile is inset by `inset` of the size."""
    S = size * SS
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    # Background tile: a diagonal gradient, clipped to a rounded square.
    pad = int(S * inset)
    tile = S - 2 * pad
    grad = Image.new("RGBA", (tile, tile))
    top = (64, 120, 255, 255)
    bottom = (20, 184, 166, 255)
    gp = grad.load()
    for y in range(tile):
        for x in range(0, tile):
            t = (x * 0.35 + y * 0.65) / tile
            gp[x, y] = lerp(top, bottom, min(1.0, max(0.0, t)))
    mask = Image.new("L", (tile, tile), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (0, 0, tile - 1, tile - 1), radius=int(tile * 0.225), fill=255
    )
    img.paste(grad, (pad, pad), mask)

    d = ImageDraw.Draw(img)
    u = tile / 100.0  # one unit = 1% of the tile
    ox, oy = pad, pad

    def P(x, y):
        return (ox + x * u, oy + y * u)

    white = (255, 255, 255, 255)
    soft = (255, 255, 255, 235)
    # Two screens, with stands.
    for left in (14, 56):
        x0, y0 = P(left, 50)
        x1, y1 = P(left + 30, 72)
        d.rounded_rectangle((x0, y0, x1, y1), radius=3.2 * u, fill=soft)
        # The screen's glass, a little darker, so it reads as a monitor.
        gx0, gy0 = P(left + 3, 53)
        gx1, gy1 = P(left + 27, 69)
        d.rounded_rectangle((gx0, gy0, gx1, gy1), radius=1.6 * u, fill=(40, 120, 200, 90))
        sx0, sy0 = P(left + 12.5, 72)
        sx1, sy1 = P(left + 17.5, 77)
        d.rectangle((sx0, sy0, sx1, sy1), fill=soft)
        bx0, by0 = P(left + 8, 77)
        bx1, by1 = P(left + 22, 80)
        d.rounded_rectangle((bx0, by0, bx1, by1), radius=1.2 * u, fill=soft)

    # The hop: a dotted arc from the left screen to the right one.
    cx, cy, r = 50, 52, 22
    for i in (0, 1, 2, 6, 7, 8):
        a = math.radians(200 + i * 17.5)
        x = cx + r * math.cos(a)
        y = cy + r * 1.0 * math.sin(a)
        dot = 2.1 if i not in (0, 8) else 1.6
        d.ellipse((*P(x - dot, y - dot), *P(x + dot, y + dot)), fill=white)

    # A cursor arrow at the top of the arc, tilted a little.
    arrow = [(0, 0), (0, 17), (4.2, 13), (7.2, 19.6), (10, 18.3), (7.1, 11.9), (12.5, 11.9)]
    tilt = math.radians(-12)
    ax, ay = 44.5, 20
    pts = []
    for x, y in arrow:
        rx = x * math.cos(tilt) - y * math.sin(tilt)
        ry = x * math.sin(tilt) + y * math.cos(tilt)
        pts.append(P(ax + rx, ay + ry))
    # A dark outline, then the white arrow.
    d.polygon(pts, fill=white, outline=(15, 40, 90, 255), width=max(1, int(1.1 * u)))

    return img.resize((size, size), Image.LANCZOS)


os.makedirs(OUT, exist_ok=True)
# macOS: Apple's grid puts an 824-point tile in a 1024 canvas.
iconset = os.path.join(OUT, "Hop.iconset")
os.makedirs(iconset, exist_ok=True)
for pts in (16, 32, 128, 256, 512):
    for scale in (1, 2):
        px = pts * scale
        icon(px, 100 / 1024).save(os.path.join(iconset, f"icon_{pts}x{pts}{'@2x' if scale == 2 else ''}.png"))
subprocess.run(["iconutil", "-c", "icns", iconset, "-o", os.path.join(OUT, "Hop.icns")], check=True)
for f in os.listdir(iconset):
    os.remove(os.path.join(iconset, f))
os.rmdir(iconset)
# Windows and the window icon: the tile fills nearly the whole square.
big = icon(256, 0.02)
big.save(os.path.join(OUT, "hop-256.png"))
big.save(os.path.join(OUT, "hop.ico"), sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
icon(1024, 100 / 1024).save(os.path.join(OUT, "hop-1024.png"))
print("ok")
