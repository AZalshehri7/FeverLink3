#!/usr/bin/env python3
"""make_cover.py: draws mod/feverlink3/_metadata/0.png, the mod's cover in the game's mod list and on mod.io.

Format from the docs: `_metadata/0.png` is the optional cover ("0.png, 1.png, ...: optional cover (0.png) and gallery images,
1920 x 1080", https://tf3-docs.transportfever.net/en/guides/mod-structure.html; "Images should be PNG files at 1920x1080 pixels",
https://wiki.transportfever3.com/doku.php?id=modding:general:moddefinition), 16:9, at most 8 MB
(https://wiki.transportfever3.com/doku.php?id=modding:general:guidelines). The same guidelines ask covers for "no text, logos, or
graphical overlays" and suggest a "clean, minimalist 2D icon" for script mods; the owner asked for the title and a subtitle on it
(2026-10-18), and `--no-text` draws the schematic alone should a mod.io review insist.

The drawing is our own: two game windows joined by a link, in two colours (navy and blue, the blue of feverlink3.exe's icon,
launcher/resources/make_icon.py), with the title in Segoe UI (Windows fonts). No game artwork, no Urban Games logo. Re-run it after
changing the drawing and commit the PNG; the release packaging copies the whole mod folder (tools/release/src/package.rs).

    python tools/release/make_cover.py [--no-text] [OUT]      (needs Pillow: pip install pillow)
"""
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

W, H = 1920, 1080
S = 2  # drawn at twice the size and scaled down: smooth edges

NAVY = (0x0F, 0x1B, 0x2D)
BLUE = (0x2F, 0x80, 0xED)
LIGHT = (0xBF, 0xD7, 0xFF)  # the blue, lightened: the text

FONTS = Path("C:/Windows/Fonts")


def font(names, size):
    for n in names:
        p = FONTS / n
        if p.exists():
            return ImageFont.truetype(str(p), size * S)
    return ImageFont.load_default(size * S)


def window(d, cx, cy, w, h, filled):
    """A game window: a rounded frame with a title bar; one of the two is filled."""
    x0, y0, x1, y1 = (cx - w // 2) * S, (cy - h // 2) * S, (cx + w // 2) * S, (cy + h // 2) * S
    r, t = 22 * S, 10 * S
    if filled:
        d.rounded_rectangle((x0, y0, x1, y1), r, fill=BLUE)
        d.rounded_rectangle((x0 + t, y0 + 46 * S, x1 - t, y1 - t), r - t, fill=NAVY)
    else:
        d.rounded_rectangle((x0, y0, x1, y1), r, outline=BLUE, width=t)
        d.rectangle((x0 + t, y0 + 46 * S - t, x1 - t, y0 + 46 * S), fill=BLUE)
    # the world inside: a road of three segments with a station, the same in both windows
    pts = [(x0 + 0.18 * (x1 - x0), y1 - 0.30 * (y1 - y0)), (x0 + 0.42 * (x1 - x0), y1 - 0.52 * (y1 - y0)),
           (x0 + 0.62 * (x1 - x0), y1 - 0.40 * (y1 - y0)), (x0 + 0.84 * (x1 - x0), y1 - 0.62 * (y1 - y0))]
    d.line(pts, fill=LIGHT if filled else BLUE, width=9 * S, joint="curve")
    for x, y in pts:
        d.ellipse((x - 13 * S, y - 13 * S, x + 13 * S, y + 13 * S), fill=LIGHT if filled else BLUE)


def draw(text=True):
    img = Image.new("RGB", (W * S, H * S), NAVY)
    d = ImageDraw.Draw(img)
    cy = 400 if text else 540
    window(d, 640, cy, 470, 330, False)
    window(d, 1280, cy, 470, 330, True)
    # the link between them: a dashed line with a dot in the middle
    y = cy * S
    for x in range(905, 1015, 36):
        d.rounded_rectangle((x * S, y - 6 * S, (x + 20) * S, y + 6 * S), 6 * S, fill=BLUE)
    if text:
        title = font(["seguisb.ttf", "segoeuib.ttf", "arialbd.ttf"], 150)
        sub = font(["segoeuil.ttf", "segoeui.ttf", "arial.ttf"], 54)
        d.text((W * S // 2, 770 * S), "FeverLink3", font=title, fill=LIGHT, anchor="mm")
        d.text((W * S // 2, 900 * S), "co-op multiplayer for Transport Fever 3", font=sub, fill=BLUE, anchor="mm")
    return img.resize((W, H), Image.LANCZOS)


def main(argv):
    text = "--no-text" not in argv
    rest = [a for a in argv if a != "--no-text"]
    out = Path(rest[0]) if rest else Path(__file__).resolve().parents[2] / "mod/feverlink3/_metadata/0.png"
    draw(text).save(out, "PNG", optimize=True)
    print(f"{out}: {W}x{H}, {out.stat().st_size} bytes")


if __name__ == "__main__":
    main(sys.argv[1:])
