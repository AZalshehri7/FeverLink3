"""Assembles the split-screen demo frames into an animated GIF.

Usage: python make_demo_gif.py FRAMES_DIR OUT.gif [--width 1280] [--colors 128]

FRAMES_DIR holds PNG frames and frames.tsv, one line per frame: file name <TAB> caption <TAB> seconds shown.
The left half of every frame is the host's game and the right half the guest's; the labels over the halves and the
caption bar at the bottom are drawn here. Needs Pillow (pip install pillow).
"""
import argparse
import os
import sys

from PIL import Image, ImageDraw, ImageFont

FONT = "C:/Windows/Fonts/segoeuib.ttf"
FONT_LIGHT = "C:/Windows/Fonts/segoeui.ttf"


def font(path, size):
    try:
        return ImageFont.truetype(path, size)
    except OSError:
        return ImageFont.load_default()


def label(draw, box, text, f, fill, text_fill=(255, 255, 255, 255)):
    x0, y0, x1, y1 = box
    draw.rounded_rectangle(box, radius=8, fill=fill)
    w = draw.textlength(text, font=f)
    draw.text((x0 + (x1 - x0 - w) / 2, y0 + 5), text, font=f, fill=text_fill)


def decorate(img, caption, width):
    scale = width / 2560.0
    img = img.convert("RGBA")
    overlay = Image.new("RGBA", img.size, (0, 0, 0, 0))
    d = ImageDraw.Draw(overlay)
    big = font(FONT, max(16, int(34 * scale)))
    cap = font(FONT_LIGHT, max(14, int(30 * scale)))
    half = img.width // 2
    pad = int(14 * scale)
    h = int(46 * scale)
    # the labels sit in the title bars of the two game windows
    label(d, (pad + int(430 * scale), pad // 2, half - pad - int(430 * scale), pad // 2 + h), "HOST  -  Anna", big, (30, 120, 70, 215))
    label(d, (half + pad + int(430 * scale), pad // 2, img.width - pad - int(430 * scale), pad // 2 + h), "GUEST  -  Ben", big, (190, 100, 30, 215))
    if caption:
        bar_h = int(54 * scale)
        d.rectangle((0, img.height - bar_h, img.width, img.height), fill=(10, 14, 20, 205))
        w = d.textlength(caption, font=cap)
        d.text(((img.width - w) / 2, img.height - bar_h + (bar_h - int(30 * scale)) / 2 - 2), caption, font=cap, fill=(240, 244, 250, 255))
    return Image.alpha_composite(img, overlay).convert("RGB")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("frames")
    ap.add_argument("out")
    ap.add_argument("--width", type=int, default=1280)
    ap.add_argument("--colors", type=int, default=128)
    args = ap.parse_args()

    rows = []
    with open(os.path.join(args.frames, "frames.tsv"), encoding="utf-8") as f:
        for line in f:
            line = line.rstrip("\n")
            if not line:
                continue
            parts = line.split("\t")
            name = parts[0]
            caption = parts[1] if len(parts) > 1 else ""
            seconds = float(parts[2]) if len(parts) > 2 else 0.5
            rows.append((name, caption, seconds))
    if not rows:
        sys.exit("no frames")

    frames, durations = [], []
    last_key = None
    for name, caption, seconds in rows:
        img = Image.open(os.path.join(args.frames, name))
        img = img.resize((args.width, int(img.height * args.width / img.width)), Image.LANCZOS)
        img = decorate(img, caption, args.width)
        key = (caption, img.tobytes()[::997])
        if frames and key == last_key:
            durations[-1] += int(seconds * 1000)
            continue
        last_key = key
        frames.append(img.quantize(colors=args.colors, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE))
        durations.append(int(seconds * 1000))
    durations[-1] += 2500  # a pause on the last picture before the loop restarts
    frames[0].save(args.out, save_all=True, append_images=frames[1:], duration=durations, loop=0, optimize=True, disposal=1)
    print("%d frames -> %s (%.1f MB)" % (len(frames), args.out, os.path.getsize(args.out) / 1e6))


if __name__ == "__main__":
    main()
