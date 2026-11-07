# Builds Keel's app icons from the artwork (keel-source.png): a rounded-square badge
# with a speedboat, speed lines and a separate fin keel under the hull cut out of it.
#
# The source is greyscale, dark badge on white.  Darkness becomes alpha, so the boat and the
# area outside the badge are transparent and every pixel carries one glyph colour; dark glyphs
# never get a light halo.
#
# Written files (python assets/make_icon.py, from any folder):
#   keel.ico        dark badge #1f1f1f: the executables' icon, tray on a light taskbar
#   keel-light.ico  white badge: tray on a dark taskbar
#   keel-256.png    dark badge at 256 px (README logo, light theme)
#   keel-256-light.png  white badge at 256 px (README logo, dark theme)
#   keel-boat.png   the boat alone, no badge, 256 px (main window logo; recoloured by the UI)
#   keel-tray.ico / keel-tray-light.ico   the boat alone, dark / white: the tray icon
#   icon-masks.a8   four 128 px alpha masks (badge, holes, ring, boat), raw bytes in that order:
#                   keel-core's appicon recolours them for the chosen icon colour and style
# ICO frames: 16, 20, 24, 32, 40, 48, 64, 128, 256 px.  Requires Python 3 and Pillow.
from pathlib import Path
from PIL import Image, ImageOps

HERE = Path(__file__).resolve().parent
SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
DARK, LIGHT = (31, 31, 31), (255, 255, 255)


def alpha(size):
    src = ImageOps.autocontrast(Image.open(HERE / "keel-source.png").convert("L"))
    return ImageOps.invert(src).resize((size, size), Image.LANCZOS)


def frame(size, rgb):
    im = Image.new("RGBA", (size, size), rgb + (0,))
    im.putalpha(alpha(size))
    return im


def boat(size):
    """The boat alone (no badge): the light pixels inside the badge, cropped and centred.
    Dark glyph, transparent elsewhere; the UI recolours it (Slint `colorize`)."""
    from PIL import ImageChops, ImageDraw
    src = ImageOps.autocontrast(Image.open(HERE / "keel-source.png").convert("L"))
    mask = src.point(lambda v: 255 if v > 128 else 0)
    w, h = mask.size
    for xy in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)]:
        ImageDraw.floodfill(mask, xy, 128)
    inside = mask.point(lambda v: 0 if v == 128 else 255)
    m = w // 12  # white slivers along the image edges are not the boat
    d = ImageDraw.Draw(inside)
    for r in ([0, 0, w, m], [0, h - m, w, h], [0, 0, m, h], [w - m, 0, w, h]):
        d.rectangle(r, fill=0)
    glyph = ImageChops.multiply(src, inside)
    glyph = glyph.crop(glyph.point(lambda v: 255 if v > 40 else 0).getbbox())
    side = max(glyph.size)
    sq = Image.new("L", (side, side), 0)
    sq.paste(glyph, ((side - glyph.size[0]) // 2, (side - glyph.size[1]) // 2))
    im = Image.new("RGBA", (size, size), DARK + (0,))
    im.putalpha(sq.resize((size, size), Image.LANCZOS))
    return im


def write_boat_ico(name, rgb):
    """Tray icons: the boat alone in one colour (no badge)."""
    frames = []
    for s in SIZES:
        im = boat(s)
        solid = Image.new("RGBA", (s, s), rgb + (0,))
        solid.putalpha(im.getchannel("A"))
        frames.append(solid)
    frames[-1].save(HERE / name, format="ICO", sizes=[(s, s) for s in SIZES], append_images=frames[:-1])


def write_ico(name, rgb):
    frames = [frame(s, rgb) for s in SIZES]
    frames[-1].save(HERE / name, format="ICO", sizes=[(s, s) for s in SIZES], append_images=frames[:-1])


def masks(size=128):
    """Alpha masks for runtime recolouring: the badge, the holes in it (boat, speed lines, keel),
    a ring along the badge's edge (outline style) and the boat alone."""
    from PIL import ImageChops, ImageFilter
    badge = alpha(size)
    inside = Image.new("L", (size, size), 0)
    px, out = badge.load(), inside.load()
    for y in range(size):  # the badge is a rounded square: fill each row between its ends
        xs = [x for x in range(size) if px[x, y] > 128]
        if xs:
            for x in range(xs[0], xs[-1] + 1):
                out[x, y] = 255
    inside = inside.filter(ImageFilter.GaussianBlur(0.6))
    holes = ImageChops.subtract(inside, badge)
    # Erode with a margin: the badge touches the image edge, where MinFilter would not erode.
    pad = ImageOps.expand(inside, border=8, fill=0).filter(ImageFilter.MinFilter(13)).crop((8, 8, size + 8, size + 8))
    ring = ImageChops.subtract(inside, pad)
    return b"".join(m.tobytes() for m in (badge, holes, ring, boat(size).getchannel("A")))


if __name__ == "__main__":
    (HERE / "icon-masks.a8").write_bytes(masks())
    write_ico("keel.ico", DARK)
    write_ico("keel-light.ico", LIGHT)
    frame(256, DARK).save(HERE / "keel-256.png", optimize=True)
    frame(256, LIGHT).save(HERE / "keel-256-light.png", optimize=True)
    boat(256).save(HERE / "keel-boat.png", optimize=True)
    write_boat_ico("keel-tray.ico", DARK)
    write_boat_ico("keel-tray-light.ico", LIGHT)
