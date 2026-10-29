"""Draws the RustPlay icon: Ferris the crab on an AirPlay screen.

Writes the app icon (icon.png, icon.ico) and three aligned layers for the
parallax tile in the UI (ui/assets/tile-*.png). Run from the repo root:
    python icons/draw_icon.py
"""
import math
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

S = 2048  # drawing size; everything is downsampled for smooth edges
C = S / 2
ROOT = Path(__file__).resolve().parent.parent


def gradient(top, bottom, size=S):
    img = Image.new("RGBA", (size, size))
    for y in range(size):
        t = y / (size - 1)
        img.paste(tuple(round(top[i] * (1 - t) + bottom[i] * t) for i in range(3)) + (255,), (0, y, size, y + 1))
    return img


def squircle_mask(inset=150, radius=430):
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle((inset, inset, S - inset, S - inset), radius=radius, fill=255)
    return mask


def background():
    layer = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    layer.paste(gradient((52, 40, 36), (16, 14, 16)), (0, 0), squircle_mask())
    glow = Image.new("L", (S, S), 0)
    ImageDraw.Draw(glow).ellipse((C - 640, C - 520, C + 640, C + 700), fill=110)
    glow = ImageChops.multiply(glow.filter(ImageFilter.GaussianBlur(220)), squircle_mask())
    layer.paste(Image.new("RGBA", (S, S), (247, 90, 20, 255)), (0, 0), glow)
    return layer


def airplay():
    """White AirPlay screen outline with its triangle."""
    layer = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    white = (255, 255, 255, 255)
    d.rounded_rectangle((370, 470, S - 370, 1430), radius=130, outline=white, width=74)
    d.rectangle((C - 190, 1340, C + 190, 1460), fill=(0, 0, 0, 0))  # gap for the triangle
    d.polygon([(C, 1290), (C + 270, 1660), (C - 270, 1660)], fill=white)
    return layer


def spiky_body(cx, cy, rx, ry, spikes, depth):
    pts = []
    n = 720
    for i in range(n):
        a = 2 * math.pi * i / n
        r = 1.0
        if math.sin(a) < -0.15:  # upper part of the shell gets little spikes
            phase = (a - math.pi) / math.pi * spikes
            bump = max(0.0, math.cos(2 * math.pi * phase)) ** 2.2
            r += depth * bump * min(1.0, (-math.sin(a) - 0.15) * 3)
        pts.append((cx + rx * r * math.cos(a), cy + ry * r * math.sin(a)))
    return pts


def claw(d, cx, cy, r, mouth_angle, color):
    d.ellipse((cx - r, cy - r, cx + r, cy + r), fill=color)
    # The notch between the pincers.
    a0 = mouth_angle - math.radians(17)
    a1 = mouth_angle + math.radians(17)
    far = r * 1.6
    d.polygon([(cx + r * 0.05 * math.cos(mouth_angle), cy),
               (cx + far * math.cos(a0), cy + far * math.sin(a0)),
               (cx + far * math.cos(a1), cy + far * math.sin(a1))], fill=(0, 0, 0, 0))


def ferris():
    """Ferris, the Rust crab (public domain mascot), drawn flat and friendly."""
    layer = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    shell = (247, 76, 0, 255)
    shade = (196, 52, 0, 255)
    cx, cy = C, 975

    # Legs: three per side, behind the body.
    for side in (-1, 1):
        for start, end in (((240, 60), (370, 190)), ((160, 120), (250, 260))):
            x0, y0 = cx + side * start[0], cy + start[1]
            x1, y1 = cx + side * end[0], cy + end[1]
            d.line((x0, y0, x1, y1), fill=shade, width=62)
            d.ellipse((x1 - 31, y1 - 31, x1 + 31, y1 + 31), fill=shade)

    # Arms and claws, raised like Ferris always does.
    for side in (-1, 1):
        d.line((cx + side * 250, cy - 20, cx + side * 400, cy - 200), fill=shade, width=56)
        claw(d, cx + side * 440, cy - 300, 118, math.radians(-90 - side * 35), shell)

    # Body: an outline pass, then the shell with a soft top-to-bottom gradient.
    d.polygon(spiky_body(cx, cy, 352, 215, 7, 0.17), fill=shade)
    body_mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(body_mask).polygon(spiky_body(cx, cy - 6, 336, 200, 7, 0.17), fill=255)
    layer.paste(gradient((255, 128, 54), (232, 70, 8)), (0, 0), body_mask)

    gloss = Image.new("L", (S, S), 0)
    ImageDraw.Draw(gloss).ellipse((cx - 220, cy - 175, cx + 200, cy - 40), fill=60)
    gloss = ImageChops.multiply(gloss.filter(ImageFilter.GaussianBlur(30)), body_mask)
    layer.paste(Image.new("RGBA", (S, S), (255, 255, 255, 255)), (0, 0), gloss)

    # Eyes and smile.
    d = ImageDraw.Draw(layer)
    for side in (-1, 1):
        ex, ey = cx + side * 92, cy - 62
        d.ellipse((ex - 50, ey - 56, ex + 50, ey + 56), fill=(24, 18, 16, 255))
        d.ellipse((ex - 30, ey - 38, ex + 2, ey - 6), fill=(255, 255, 255, 255))
    d.arc((cx - 70, cy - 20, cx + 70, cy + 70), start=25, end=155, fill=(110, 30, 0, 255), width=18)
    return layer


def rim(width=22):
    """The squircle's edge as a thin ring, used in the UI to catch a moving light."""
    inner = Image.new("L", (S, S), 0)
    ImageDraw.Draw(inner).rounded_rectangle(
        (150 + width, 150 + width, S - 150 - width, S - 150 - width), radius=430 - width, fill=255
    )
    ring = ImageChops.subtract(squircle_mask(), inner.filter(ImageFilter.GaussianBlur(6)))
    layer = Image.new("RGBA", (S, S), (255, 255, 255, 0))
    layer.putalpha(ring)
    return layer


def down(img, size):
    return img.resize((size, size), Image.LANCZOS)


bg, ap, crab = background(), airplay(), ferris()
icon = Image.alpha_composite(Image.alpha_composite(bg, ap), crab)

down(icon, 512).save(ROOT / "icons" / "icon.png")
down(icon, 1024).save(ROOT / "icons" / "icon.ico", sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])

assets = ROOT / "ui" / "assets"
assets.mkdir(exist_ok=True)
for name, layer in (("bg", bg), ("airplay", ap), ("crab", crab), ("rim", rim())):
    down(layer, 512).save(assets / f"tile-{name}.png")
print("icon written")
