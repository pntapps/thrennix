#!/usr/bin/env python3
"""Generate Thrennix toolbar icons (16/32/48/128 px PNG) using only the standard library.

Design: a blue rounded square with a white down-arrow over a short bar.
Anti-aliased by 4x4 supersampling. Run from anywhere:

    python extension/tools/make_icons.py
"""
import os
import struct
import zlib

SIZES = (16, 32, 48, 128)
SS = 4  # supersampling factor per axis
BG = (29, 99, 237)  # strong blue
FG = (255, 255, 255)


def inside_rounded_rect(x, y, x0, y0, x1, y1, r):
    if x < x0 or x > x1 or y < y0 or y > y1:
        return False
    cx = min(max(x, x0 + r), x1 - r)
    cy = min(max(y, y0 + r), y1 - r)
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def inside_polygon(x, y, pts):
    inside = False
    n = len(pts)
    j = n - 1
    for i in range(n):
        xi, yi = pts[i]
        xj, yj = pts[j]
        if (yi > y) != (yj > y):
            xint = (xj - xi) * (y - yi) / (yj - yi) + xi
            if x < xint:
                inside = not inside
        j = i
    return inside


# Glyph in unit coordinates (0..1): down arrow + base bar.
ARROW = [
    (0.40, 0.18), (0.60, 0.18), (0.60, 0.46), (0.76, 0.46),
    (0.50, 0.70), (0.24, 0.46), (0.40, 0.46),
]


def sample(u, v):
    """Return (coverage_bg, coverage_fg) for a point in unit space."""
    if not inside_rounded_rect(u, v, 0.02, 0.02, 0.98, 0.98, 0.22):
        return None
    if inside_polygon(u, v, ARROW) or inside_rounded_rect(u, v, 0.24, 0.76, 0.76, 0.84, 0.04):
        return FG
    return BG


def render(size):
    rows = []
    n = SS * SS
    for py in range(size):
        row = bytearray([0])  # filter type None
        for px in range(size):
            r = g = b = a = 0
            for sy in range(SS):
                for sx in range(SS):
                    u = (px + (sx + 0.5) / SS) / size
                    v = (py + (sy + 0.5) / SS) / size
                    c = sample(u, v)
                    if c is not None:
                        r += c[0]
                        g += c[1]
                        b += c[2]
                        a += 1
            if a:
                row += bytes((r // a, g // a, b // a, round(255 * a / n)))
            else:
                row += b"\x00\x00\x00\x00"
        rows.append(bytes(row))
    return b"".join(rows)


def png(size, raw):
    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def main():
    out_dir = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "icons")
    os.makedirs(out_dir, exist_ok=True)
    for s in SIZES:
        path = os.path.join(out_dir, "icon-%d.png" % s)
        with open(path, "wb") as f:
            f.write(png(s, render(s)))
        print("wrote", os.path.normpath(path))


if __name__ == "__main__":
    main()
