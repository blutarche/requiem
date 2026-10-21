#!/usr/bin/env python3
"""Draws assets/extension-icon.png: a 512x512 dark rounded square with the skull mark
from scripts/make-app-icon.swift (same geometry at half the 1024 app-icon scale)."""
import math
import os
from PIL import Image, ImageDraw

SIZE = 512
SCALE = 4  # supersample for smooth edges
S = SIZE * SCALE

img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
d = ImageDraw.Draw(img)

# Background: vertical gradient clipped to a rounded square.
bg = Image.new("RGBA", (S, S))
bd = ImageDraw.Draw(bg)
top, bottom = (58, 28, 84), (18, 12, 32)
for y in range(S):
    t = y / (S - 1)
    bd.line([(0, y), (S, y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)) + (255,))
mask = Image.new("L", (S, S), 0)
ImageDraw.Draw(mask).rounded_rectangle([0, 0, S - 1, S - 1], radius=int(S * 0.225), fill=255)
img.paste(bg, (0, 0), mask)

# Skull glyph: local y-down units centred on the cranium dome, mapped like the app icon.
K = 8.3 * 0.5 * SCALE
OX, OY = S / 2, S / 2 - 9 * K
N = 96  # points per curve


def pt(x, y):
    return (OX + K * x, OY + K * y)


def cubic(p0, p1, p2, p3):
    out = []
    for i in range(1, N + 1):
        t = i / N
        u = 1 - t
        out.append((u**3 * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t**3 * p3[0],
                    u**3 * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t**3 * p3[1]))
    return out


def quad(p0, p1, p2):
    return [((1 - t) ** 2 * p0[0] + 2 * (1 - t) * t * p1[0] + t * t * p2[0],
             (1 - t) ** 2 * p0[1] + 2 * (1 - t) * t * p1[1] + t * t * p2[1])
            for t in (i / N for i in range(1, N + 1))]


def ellipse(cx, cy, rx, ry, degrees):
    a = math.radians(degrees)
    ca, sa = math.cos(a), math.sin(a)
    out = []
    for i in range(4 * N):
        t = 2 * math.pi * i / (4 * N)
        x, y = rx * math.cos(t), ry * math.sin(t)
        out.append((cx + x * ca - y * sa, cy + x * sa + y * ca))
    return out


body = [(-27 * math.cos(math.pi * i / (4 * N)), -27 * math.sin(math.pi * i / (4 * N))) for i in range(4 * N + 1)]
body += cubic((27, 0), (27, 9), (24, 14), (18, 19))
body += quad((18, 19), (16, 23), (12, 23))
body += [(-12, 23)]
body += quad((-12, 23), (-16, 23), (-18, 19))
body += cubic((-18, 19), (-24, 14), (-27, 9), (-27, 0))
nose = [(0, 12), (-3.4, 18.5)] + quad((-3.4, 18.5), (0, 19.6), (3.4, 18.5))

glyph = Image.new("L", (S, S), 0)
gd = ImageDraw.Draw(glyph)
gd.polygon([pt(*p) for p in body], fill=255)
for hole in (nose, ellipse(-10.5, 4, 7.2, 8.6, 12), ellipse(10.5, 4, 7.2, 8.6, -12)):
    gd.polygon([pt(*p) for p in hole], fill=0)
for i, h in enumerate([12, 18, 15, 10]):
    x0 = -18 + i * 9.5
    xa, ya = pt(x0, 45 - h)
    xb, yb = pt(x0 + 7.5, 45)
    gd.rounded_rectangle([xa, ya, xb, yb], radius=3.75 * K, fill=255)
img.paste(Image.new("RGBA", (S, S), (235, 228, 245, 255)), (0, 0), glyph)

out = os.path.join(os.path.dirname(__file__), "..", "assets", "extension-icon.png")
img.resize((SIZE, SIZE), Image.LANCZOS).save(out)
print(os.path.normpath(out))
