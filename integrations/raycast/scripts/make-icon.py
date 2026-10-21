#!/usr/bin/env python3
"""Draws assets/extension-icon.png: a 512x512 dark rounded square with a ring and a play triangle."""
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

# Ring and play triangle.
c = S / 2
r = S * 0.30
d.ellipse([c - r, c - r, c + r, c + r], outline=(235, 228, 245, 255), width=int(S * 0.028))
tri = [(c - r * 0.30, c - r * 0.52), (c - r * 0.30, c + r * 0.52), (c + r * 0.60, c)]
d.polygon(tri, fill=(235, 228, 245, 255))

out = os.path.join(os.path.dirname(__file__), "..", "assets", "extension-icon.png")
img.resize((SIZE, SIZE), Image.LANCZOS).save(out)
print(os.path.normpath(out))
