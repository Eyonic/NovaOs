#!/usr/bin/env python3
"""Make Nova's icon and Home poster for each Craft app from the app's own icon.

The icons are the apps' (https://github.com/storytold/<app>, MIT OR Apache-2.0; see
assets/README.md), downloaded from each repository's assets/app-icon/hicolor/256x256/
into the cache. For each app this writes:

  assets/icons/craft_<app>.png  128x128, the tile without its transparent corners (filled
                                with the tile's edge colour, as Nova's icons are; the
                                desktop rounds the corners itself)
  assets/home/craft_<app>.png   352x500 placeholder capsule and
  assets/home/hero_craft_<app>.png  768x480 hero, in Home's key-art layout (assets/README.md):
                                a black studio, a haze of the brand colour (the tile's edge),
                                the icon as the one object with a soft floor reflection

The posters are deliberately plain and kept in their own files so Home's art direction can
replace them. Needs Pillow.

Usage: make-craft-art.py [app ...]      (default: every Craft app)
"""
import os
import sys
import urllib.request

from PIL import Image, ImageDraw, ImageFilter

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CACHE = os.path.join(os.environ.get("NOVA_CRAFT_CACHE", "/tmp/nova-craft-cache"), "icons")
APPS = ["wordcraft", "gridcraft", "deckcraft", "pdfcraft", "photocraft", "lightcraft", "filmcraft", "soundcraft",
        "vectorcraft", "designcraft", "figcraft", "cadcraft", "effectcraft", "craft-launcher"]


def fetch(app):
    os.makedirs(CACHE, exist_ok=True)
    path = os.path.join(CACHE, f"{app}-256.png")
    if not os.path.exists(path):
        icon_id = "artcraft-launcher" if app == "craft-launcher" else app
        url = f"https://raw.githubusercontent.com/storytold/{app}/HEAD/assets/app-icon/hicolor/256x256/apps/ai.storyteller.{icon_id}.png"
        with urllib.request.urlopen(url, timeout=60) as r:
            data = r.read()
        with open(path + ".part", "wb") as f:
            f.write(data)
        os.replace(path + ".part", path)
    return Image.open(path).convert("RGBA")


def edge_colour(im):
    """The tile's colour: the most common opaque colour along its inner edge."""
    w, h = im.size
    counts = {}
    for i in range(w):
        for x, y in ((i, h // 2 // 8), (i, h - 1 - h // 16), (w // 16, i * h // w), (w - 1 - w // 16, i * h // w)):
            r, g, b, a = im.getpixel((x, y))
            if a > 250:
                key = (r // 8 * 8, g // 8 * 8, b // 8 * 8)
                counts[key] = counts.get(key, 0) + 1
    return max(counts, key=counts.get) if counts else (90, 90, 110)


def icon(im, colour):
    tile = Image.new("RGBA", im.size, colour + (255,))
    tile.alpha_composite(im)
    return tile.convert("RGB").resize((128, 128), Image.LANCZOS)


def mix(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def studio(w, h, colour, cx, cy, size, im):
    """Home's key-art stage (assets/README.md): a black studio, a haze of the app's colour
    behind the subject, the icon as the one object, and its soft reflection on the floor."""
    out = Image.new("RGB", (w, h), (0, 0, 0))
    haze = Image.new("L", (w, h), 0)
    r = size * 0.95
    ImageDraw.Draw(haze).ellipse((cx - r, cy - r * 0.85, cx + r, cy + r * 0.85), fill=150)
    haze = haze.filter(ImageFilter.GaussianBlur(size * 0.45))
    out = Image.composite(Image.new("RGB", (w, h), mix(colour, (0, 0, 0), 0.25)), out, haze)
    big = im.resize((size, size), Image.LANCZOS)
    x, y = cx - size // 2, cy - size // 2
    # Reflection: the icon flipped below it, fading out.
    refl = big.transpose(Image.FLIP_TOP_BOTTOM)
    fade = Image.linear_gradient("L").resize((size, size)).point(lambda v: max(0, 70 - v * 70 // 140))
    refl.putalpha(Image.composite(fade, Image.new("L", (size, size), 0), refl.getchannel("A")))
    out = out.convert("RGBA")
    out.alpha_composite(refl, (x, y + size + size // 24))
    out.alpha_composite(big, (x, y))
    return out.convert("RGB")


def capsule(im, colour):
    # 352x500: the subject centred in the upper two thirds, about two thirds of the width;
    # the bottom quarter stays dark for the title.
    return studio(352, 500, colour, 176, 200, 220, im)


def hero(im, colour):
    # 768x480: the subject a third of the height at two thirds of the width; the left half
    # stays black for the hero text.
    return studio(768, 480, colour, 512, 210, 170, im)


def main():
    apps = sys.argv[1:] or APPS
    for app in apps:
        im = fetch(app)
        colour = edge_colour(im)
        icon(im, colour).save(os.path.join(ROOT, "assets/icons", f"craft_{app}.png"), optimize=True)
        for name, art in ((f"craft_{app}", capsule), (f"hero_craft_{app}", hero)):
            art(im, colour).quantize(colors=192, method=Image.MEDIANCUT, dither=Image.FLOYDSTEINBERG).convert("RGB").save(
                os.path.join(ROOT, "assets/home", f"{name}.png"), optimize=True)
        print(f"{app}: colour #{colour[0]:02X}{colour[1]:02X}{colour[2]:02X}")


if __name__ == "__main__":
    main()
