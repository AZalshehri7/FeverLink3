"""Assembles the split-screen demo frames into an animated GIF.

Usage: python make_demo_gif.py FRAMES_DIR OUT.gif [--width 1280] [--colors 255] [--skip f0089,f0091] [--webp OUT.webp]

FRAMES_DIR holds PNG frames and frames.tsv, one line per frame: file name <TAB> caption <TAB> seconds shown
<TAB> optional mouse position x <TAB> y (screen pixels of the 2560 wide desktop; a ring is drawn there).
The left half of every frame is the host's game and the right half the guest's; the labels over the halves and the
caption bar at the bottom are drawn here. Needs Pillow (pip install pillow).
"""
import argparse
import os
import sys

from PIL import Image, ImageDraw, ImageFont, ImageStat

MAX_HOLD_MS = 1600  # longest a picture that did not change is shown
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


def decorate(img, caption, width, mouse=None):
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
    if mouse:
        mx, my = mouse[0] * scale, mouse[1] * scale
        r = max(9, int(22 * scale))
        d.ellipse((mx - r, my - r, mx + r, my + r), outline=(255, 214, 10, 255), width=max(2, int(5 * scale)))
        d.ellipse((mx - 3, my - 3, mx + 3, my + 3), fill=(255, 214, 10, 255))
    if caption:
        bar_h = int(54 * scale)
        d.rectangle((0, img.height - bar_h, img.width, img.height), fill=(10, 14, 20, 205))
        w = d.textlength(caption, font=cap)
        d.text(((img.width - w) / 2, img.height - bar_h + (bar_h - int(30 * scale)) / 2 - 2), caption, font=cap, fill=(240, 244, 250, 255))
    return Image.alpha_composite(img, overlay).convert("RGB")


def signature(img):
    """A small grey picture of the whole frame and of the two bottom bars (date and speed of both games): two pictures
    count as the same when neither changed visibly. The game's own FPS counter and moving trees do not matter."""
    w, h = img.size
    whole = img.convert("L").resize((64, 36), Image.BILINEAR)
    # the bottom bar of each game (speed buttons and date), at full size: a changing digit is a handful of pixels
    left = img.crop((int(w * 0.40), int(h * 0.966), int(w * 0.505), int(h * 0.992))).convert("L")
    right = img.crop((int(w * 0.90), int(h * 0.966), w, int(h * 0.992))).convert("L")
    return (list(whole.tobytes()), list(left.tobytes()) + list(right.tobytes()))


def mean_abs_diff(a, b):
    return sum(abs(x - y) for x, y in zip(a, b)) / max(1, len(a))


def same_picture(sig, last, caption, mouse):
    if last is None or last[2] != caption or last[3] != mouse:
        return False
    bar_changes = sum(1 for x, y in zip(sig[1], last[1]) if abs(x - y) > 30)   # the date digits changing
    return mean_abs_diff(sig[0], last[0]) < 1.0 and bar_changes < 3


def has_gamebar_popup(img):
    """The Windows Game Bar sometimes shows a dark "Ready to clip" panel over the top left of the first game window. It is
    not part of the demo, so such pictures are left out: a dark patch there while the rest of that window is bright
    (a loading screen is dark everywhere, so it is not mistaken for it)."""
    w, h = img.size
    patch = img.crop((int(w * 0.012), int(h * 0.03), int(w * 0.11), int(h * 0.18))).convert("L")
    rest = img.crop((int(w * 0.19), int(h * 0.70), int(w * 0.48), int(h * 0.95))).convert("L")
    return ImageStat.Stat(patch).mean[0] < 70 and ImageStat.Stat(rest).mean[0] > 85


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("frames")
    ap.add_argument("out")
    ap.add_argument("--width", type=int, default=1280)
    ap.add_argument("--colors", type=int, default=255)
    ap.add_argument("--webp", default="", help="also write an animated WebP (much smaller than the GIF, same pictures)")
    ap.add_argument("--skip", default="", help="frame names (without .png) to leave out, comma separated")
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
            mouse = (float(parts[3]), float(parts[4])) if len(parts) > 4 and parts[3] else None
            rows.append((name, caption, seconds, mouse))
    if not rows:
        sys.exit("no frames")

    skip = {x.strip() for x in args.skip.split(",") if x.strip()}
    rows = [r for r in rows if os.path.splitext(r[0])[0] not in skip]

    # decorate every picture once (labels, caption, pointer ring), then give all of them one shared palette: static
    # parts of the screen then stay identical from picture to picture and the GIF only stores what changed
    pictures = []
    skipped = 0
    for name, caption, seconds, mouse in rows:
        img = Image.open(os.path.join(args.frames, name))
        if has_gamebar_popup(img):
            skipped += 1
            continue
        img = img.resize((args.width, int(img.height * args.width / img.width)), Image.LANCZOS)
        pictures.append((decorate(img, caption, args.width, mouse), caption, seconds, mouse))

    samples = pictures[:: max(1, len(pictures) // 24)]
    w, h = samples[0][0].size
    sheet = Image.new("RGB", (w, h * len(samples)))
    for i, (im, _, _, _) in enumerate(samples):
        sheet.paste(im, (0, i * h))
    palette = sheet.quantize(colors=args.colors, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE)

    frames, durations, kept = [], [], []
    last_sig = None
    for img, caption, seconds, mouse in pictures:
        sig = signature(img)
        if frames and same_picture(sig, last_sig, caption, mouse):
            # a long wait (the world being sent, a loading screen) is shown shortened
            durations[-1] = min(durations[-1] + int(seconds * 1000), MAX_HOLD_MS)
            continue
        last_sig = sig + (caption, mouse)
        kept.append(img)
        frames.append(img.quantize(palette=palette, dither=Image.Dither.NONE))
        durations.append(int(seconds * 1000))
    durations[-1] += 2500  # a pause on the last picture before the loop restarts
    frames[0].save(args.out, save_all=True, append_images=frames[1:], duration=durations, loop=0, optimize=True, disposal=1)
    if args.webp:
        kept[0].save(args.webp, save_all=True, append_images=kept[1:], duration=durations, loop=0, quality=72, method=6)
        print("webp -> %s (%.1f MB)" % (args.webp, os.path.getsize(args.webp) / 1e6))
    print("%d frames (%d pictures with the Game Bar panel left out) -> %s (%.1f MB)" % (len(frames), skipped, args.out, os.path.getsize(args.out) / 1e6))


if __name__ == "__main__":
    main()
