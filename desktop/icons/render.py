#!/usr/bin/env python3
"""Render the desktop icons (32x32.png, 128x128.png) from the shapes below.

    python3 desktop/icons/render.py

The tray and the window take 32x32.png, the launcher takes both; they are the
only artwork the desktop host ships (`bundle.icon` in tauri.conf.json). The
output depends on nothing but this file, so rerunning it reproduces the same
bytes. Colours are the console's: `--panel`, `--line` and `--sig`.
"""

import math
import struct
import zlib
from pathlib import Path

SIZES = (32, 128)
CANVAS = 128
SAMPLES = 4
PANEL = (0x14, 0x15, 0x17)
LINE = (0x30, 0x34, 0x3A)
SIGNAL = (0xFF, 0xB6, 0x5C)


def rounded(x, y, centre, size, radius):
    """Signed distance to a rounded rectangle (negative inside)."""
    dx = abs(x - centre[0]) - (size[0] / 2 - radius)
    dy = abs(y - centre[1]) - (size[1] / 2 - radius)
    return math.hypot(max(dx, 0), max(dy, 0)) + min(max(dx, dy), 0) - radius


def sample(x, y):
    """The colour at a point of the canvas, or `None` outside the plate."""
    plate = rounded(x, y, (CANVAS / 2, CANVAS / 2), (CANVAS, CANVAS), 28)
    if plate > 0:
        return None
    # Microphone: capsule, cradle, stem and foot.
    if rounded(x, y, (64, 47), (28, 46), 14) <= 0:
        return SIGNAL
    ring = abs(math.hypot(x - 64, y - 50) - 30) <= 4.5
    legs = 44 <= y <= 50 and (30 <= x <= 39 or 89 <= x <= 98)
    if (ring and y >= 50) or legs or (59.5 <= x <= 68.5 and 80 <= y <= 96):
        return SIGNAL
    if rounded(x, y, (64, 100), (36, 9), 4.5) <= 0:
        return SIGNAL
    return LINE if plate > -3 else PANEL


def render(size):
    """RGBA rows of the icon at `size`, each pixel covered by 4x4 samples."""
    count = SAMPLES * SAMPLES
    pixels = bytearray()
    for py in range(size):
        for px in range(size):
            red = green = blue = cover = 0
            for sy in range(SAMPLES):
                for sx in range(SAMPLES):
                    x = (px + (sx + 0.5) / SAMPLES) * CANVAS / size
                    y = (py + (sy + 0.5) / SAMPLES) * CANVAS / size
                    colour = sample(x, y)
                    if colour is not None:
                        red, green, blue = red + colour[0], green + colour[1], blue + colour[2]
                        cover += 1
            if cover:
                # Premultiplied average: no dark fringe at the transparent edge.
                pixels += bytes((round(red / cover), round(green / cover), round(blue / cover),
                                 round(255 * cover / count)))
            else:
                pixels += b"\0\0\0\0"
    return bytes(pixels)


def encode(size, pixels):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    rows = b"".join(b"\0" + pixels[y * size * 4:(y + 1) * size * 4] for y in range(size))
    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(rows, 9)) + chunk(b"IEND", b"")


def main():
    for size in SIZES:
        path = Path(__file__).with_name(f"{size}x{size}.png")
        path.write_bytes(encode(size, render(size)))
        print(path)


if __name__ == "__main__":
    main()
