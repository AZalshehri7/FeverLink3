#!/usr/bin/env python3
"""make_icon.py: draws feverlink3.ico, the icon of feverlink3.exe. Pure Python, no packages.

The picture is the "two people" glyph of the mod's in-game button (mod/feverlink3/content/feverlink3_icon.tga),
white on a blue rounded square, drawn from shapes instead of scaled from the 64 pixel bitmap, so that every size is
sharp. Re-run it after changing the drawing and commit the new .ico; the build (launcher/build.rs) only reads the
.ico, so nobody needs Python to build FeverLink3.

    python -I launcher/resources/make_icon.py [--preview DIR]

Sizes 16 to 64 are stored as 32-bit bitmaps with an alpha channel, 256 as PNG (Windows Vista and later), the
usual mix. `--preview DIR` also writes one PNG per size for looking at.
"""
import math
import struct
import sys
import zlib
from pathlib import Path

SIZES = [16, 24, 32, 48, 64, 256]
SAMPLES = 4  # sub-samples per pixel and axis: anti-aliasing by coverage

TOP = (0x2F, 0x80, 0xED)  # background, top
BOTTOM = (0x1D, 0x4E, 0xD8)  # background, bottom
FRONT = (0xFF, 0xFF, 0xFF)
BACK = (0xBF, 0xD7, 0xFF)


def in_rounded_square(x, y):
    """The badge: a square with a small margin and rounded corners."""
    half, radius = 0.46, 0.22
    qx, qy = abs(x - 0.5) - (half - radius), abs(y - 0.5) - (half - radius)
    return math.hypot(max(qx, 0.0), max(qy, 0.0)) + min(max(qx, qy), 0.0) - radius <= 0.0


def in_circle(x, y, cx, cy, r):
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def in_dome(x, y, cx, cy, rx, ry, floor):
    """The upper part of an ellipse, cut off flat at `floor`: a pair of shoulders."""
    return y <= floor and ((x - cx) / rx) ** 2 + ((y - cy) / ry) ** 2 <= 1.0


def person(x, y, head, body, grow=0.0):
    """True inside a person; `grow` widens the outline, used to leave a gap around the person in front."""
    (hx, hy, hr), (bx, by, brx, bry, floor) = head, body
    return in_circle(x, y, hx, hy, hr + grow) or in_dome(x, y, bx, by, brx + grow, bry + grow, floor + grow)


FRONT_PERSON = ((0.575, 0.40, 0.115), (0.575, 0.84, 0.245, 0.30, 0.80))
BACK_PERSON = ((0.335, 0.37, 0.085), (0.31, 0.80, 0.17, 0.24, 0.80))


def sample(x, y):
    """The colour at one point of the unit square, or None outside the badge."""
    if not in_rounded_square(x, y):
        return None
    if person(x, y, *FRONT_PERSON):
        return FRONT
    if person(x, y, *BACK_PERSON) and not person(x, y, *FRONT_PERSON, grow=0.028):
        return BACK
    t = min(max((y - 0.04) / 0.92, 0.0), 1.0)
    return tuple(round(a + (b - a) * t) for a, b in zip(TOP, BOTTOM))


def render(size):
    """size x size pixels of (r, g, b, a), top row first."""
    rows = []
    n = SAMPLES
    for py in range(size):
        row = []
        for px in range(size):
            r = g = b = hits = 0
            for sy in range(n):
                for sx in range(n):
                    c = sample((px + (sx + 0.5) / n) / size, (py + (sy + 0.5) / n) / size)
                    if c is not None:
                        r, g, b, hits = r + c[0], g + c[1], b + c[2], hits + 1
            if hits == 0:
                row.append((0, 0, 0, 0))
            else:
                row.append((round(r / hits), round(g / hits), round(b / hits), round(255 * hits / (n * n))))
        rows.append(row)
    return rows


def png(size, rows):
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    raw = b"".join(b"\x00" + b"".join(bytes(p) for p in row) for row in rows)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def bitmap(size, rows):
    """The image of an icon directory entry: a DIB (no file header), height doubled for the AND mask."""
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    pixels = b"".join(b"".join(struct.pack("<BBBB", b, g, r, a) for r, g, b, a in row) for row in reversed(rows))
    stride = ((size + 31) // 32) * 4
    mask = bytearray()
    for row in reversed(rows):  # a set bit is "transparent"; the alpha channel does the real work
        bits = bytearray(stride)
        for x, (_, _, _, a) in enumerate(row):
            if a == 0:
                bits[x // 8] |= 0x80 >> (x % 8)
        mask += bits
    return header + pixels + bytes(mask)


def ico(images):
    """images: [(size, bytes)] -> the .ico file."""
    out = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    for size, data in images:
        side = 0 if size >= 256 else size
        out += struct.pack("<BBBBHHII", side, side, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    return out + b"".join(data for _, data in images)


def main():
    preview = Path(sys.argv[sys.argv.index("--preview") + 1]) if "--preview" in sys.argv else None
    images = []
    for size in SIZES:
        rows = render(size)
        images.append((size, png(size, rows) if size >= 256 else bitmap(size, rows)))
        if preview:
            preview.mkdir(parents=True, exist_ok=True)
            (preview / f"icon-{size}.png").write_bytes(png(size, rows))
    target = Path(__file__).with_name("feverlink3.ico")
    target.write_bytes(ico(images))
    print(f"wrote {target} ({target.stat().st_size} bytes, sizes {SIZES})")


if __name__ == "__main__":
    main()
