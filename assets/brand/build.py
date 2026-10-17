"""Renders Ferry's app icon and tray icons from the brand geometry, and prints theme tokens.

    python assets/brand/build.py

Writes ferry.ico, ferry-512.png and tray-<state>-<taskbar>.ico next to this file. Needs Pillow.
Colors are OKLCH (the same values as DESIGN.md and the mockups), converted here so there is one source.
"""
import math
from pathlib import Path

from PIL import Image, ImageDraw

OUT = Path(__file__).parent
SS = 8  # supersampling factor


def oklch(l, c, h, a=1.0):
    """OKLCH -> sRGB 0-255 tuple (clipped)."""
    hr = math.radians(h)
    L, A, B = l, c * math.cos(hr), c * math.sin(hr)
    l_ = (L + 0.3963377774 * A + 0.2158037573 * B) ** 3
    m_ = (L - 0.1055613458 * A - 0.0638541728 * B) ** 3
    s_ = (L - 0.0894841775 * A - 1.2914855480 * B) ** 3
    lin = (
        4.0767416621 * l_ - 3.3077115913 * m_ + 0.2309699292 * s_,
        -1.2684380046 * l_ + 2.6097574011 * m_ - 0.3413193965 * s_,
        -0.0041960863 * l_ - 0.7034186147 * m_ + 1.7076147010 * s_,
    )
    enc = lambda x: 12.92 * x if x <= 0.0031308 else 1.055 * x ** (1 / 2.4) - 0.055
    return tuple(round(min(1, max(0, enc(x))) * 255) for x in lin) + (round(a * 255),)


INK_HI, INK, INK_LO = oklch(0.38, 0.03, 250), oklch(0.27, 0.02, 250), oklch(0.2, 0.02, 250)
AMBER = oklch(0.78, 0.15, 72)
AMBER_DEEP = oklch(0.66, 0.16, 62)  # reads on a light taskbar
WHITE = (255, 255, 255, 255)
RED = oklch(0.58, 0.2, 27)


def hull(d, k, ox, oy, color, cargo, water=True):
    """The mark on a 24-unit grid, scaled by k and offset. cargo colors the top block."""
    p = lambda x, y: (ox + x * k, oy + y * k)
    d.rounded_rectangle([p(6, 5), p(13, 8.4)], radius=k, fill=cargo)
    d.rounded_rectangle([p(6, 9.6), p(18, 13)], radius=k, fill=color)
    pts = [p(2.8, 14.6), p(21.2, 14.6), p(18.6, 19.2), p(5.4, 19.2)]
    d.polygon(pts, fill=color)
    d.line(pts + pts[:2], fill=color, width=round(k), joint="curve")  # wrap past the start so every corner gets a joint
    if water:
        wave = [p(3 + i * 0.25, 21.6 - 0.6 * abs(math.sin(math.pi * i * 0.25 / 4.5))) for i in range(73)]
        c = color[:3] + (round(color[3] * 0.7),)
        d.line(wave, fill=c, width=round(1.4 * k), joint="curve")
        for e in (wave[0], wave[-1]):
            r = 0.7 * k
            d.ellipse([e[0] - r, e[1] - r, e[0] + r, e[1] + r], fill=c)


def tray_glyph(d, k, color, cargo):
    """Pixel-aligned 16-grid version for the tray: no curves to blur at 16 px."""
    p = lambda x, y: (x * k, y * k)
    d.rounded_rectangle([p(4, 2), p(9, 5)], radius=0.6 * k, fill=cargo)
    d.rounded_rectangle([p(4, 6), p(12, 9)], radius=0.6 * k, fill=color)
    d.polygon([p(1, 10), p(15, 10), p(13, 13.5), p(3, 13.5)], fill=color)
    c = color[:3] + (round(color[3] * 0.7),)
    for x in (2, 7, 12):
        d.rectangle([p(x, 14.5), p(x + 2.5, 15.5)], fill=c)


def app_icon(size):
    s = size * SS
    img = Image.new("RGBA", (s, s))
    # Vertical ink gradient on the plate.
    grad = Image.new("RGBA", (1, s))
    for y in range(s):
        t = y / (s - 1)
        a, b, u = (INK_HI, INK, t / 0.55) if t < 0.55 else (INK, INK_LO, (t - 0.55) / 0.45)
        grad.putpixel((0, y), tuple(round(a[i] + (b[i] - a[i]) * u) for i in range(4)))
    grad = grad.resize((s, s))
    m = s * (1 / 32 if size >= 32 else 0)
    mask = Image.new("L", (s, s))
    ImageDraw.Draw(mask).rounded_rectangle([m, m, s - m, s - m], radius=(s - 2 * m) * 0.22, fill=255)
    img.paste(grad, (0, 0), mask)
    d = ImageDraw.Draw(img)
    if size <= 24:  # small sizes use the tray grid, without water
        k = s * 0.72 / 16
        o = (s - 16 * k) / 2
        sub = Image.new("RGBA", (s, s))
        sd = ImageDraw.Draw(sub)
        tray_glyph(sd, k, WHITE, AMBER)
        sd.rectangle([0, 14.2 * k, s, s], fill=(0, 0, 0, 0))
        img.alpha_composite(sub, (round(o), round(o + 1.2 * k)))
    else:
        # Center the mark's visible bounds (x 2.3..21.7, y 5..22.3 on the 24 grid, strokes included),
        # not the 24-unit grid box, which would leave more room above the cargo than below the water.
        k = s * 0.72 / 24
        hull(d, k, s / 2 - 12 * k, s / 2 - 13.65 * k, WHITE, AMBER)
    return img.resize((size, size), Image.LANCZOS)


def tray_icon(size, state, taskbar):
    s = size * SS
    k = s / 16
    img = Image.new("RGBA", (s, s))
    fg = WHITE if taskbar == "dark" else oklch(0.2, 0.01, 250)
    cargo = (AMBER if taskbar == "dark" else AMBER_DEEP) if state == "carrying" else fg
    tray_glyph(ImageDraw.Draw(img), k, fg, cargo)
    if state in ("attention", "paused"):
        # Cut a ring out of the glyph, then draw the badge bottom-right like a Windows overlay.
        cx = cy = 12.5 * k
        cut = Image.new("L", (s, s), 255)
        ImageDraw.Draw(cut).ellipse([cx - 4.5 * k, cy - 4.5 * k, cx + 4.5 * k, cy + 4.5 * k], fill=0)
        img.putalpha(Image.composite(img.getchannel("A"), cut, cut))
        d = ImageDraw.Draw(img)
        if state == "attention":
            d.ellipse([cx - 3.5 * k, cy - 3.5 * k, cx + 3.5 * k, cy + 3.5 * k], fill=RED)
        else:
            for x in (10.5, 13.3):
                d.rectangle([x * k, 9.5 * k, (x + 1.4) * k, 15.5 * k], fill=fg)
    return img.resize((size, size), Image.LANCZOS)


def brand_font(px, style="Semibold"):
    """Segoe UI Variable Display (Semibold is the wordmark), falling back to Segoe UI."""
    from PIL import ImageFont
    fonts = Path(r"C:\Windows\Fonts")
    try:
        f = ImageFont.truetype(str(fonts / "SegUIVar.ttf"), px)
        f.set_variation_by_name(f"{style} Display")
        return f
    except Exception:
        return ImageFont.truetype(str(fonts / ("seguisb.ttf" if style == "Semibold" else "segoeui.ttf")), px)


def ink_gradient(w, h):
    grad = Image.new("RGBA", (1, h))
    for y in range(h):
        t = y / (h - 1)
        a, b, u = (INK_HI, INK, t / 0.55) if t < 0.55 else (INK, INK_LO, (t - 0.55) / 0.45)
        grad.putpixel((0, y), tuple(round(a[i] + (b[i] - a[i]) * u) for i in range(4)))
    return grad.resize((w, h))


def installer_side(scale=1.0, w=164, h=314):
    """NSIS welcome/finish page panel at one display scale (164x314 px at 100%).
    The installer picks the file matching the screen's DPI, so nothing is resized at run time."""
    ow, oh = round(w * scale), round(h * scale)
    W, H = ow * 4, oh * 4  # supersample
    img = ink_gradient(W, H)
    # Faint water across the bottom, blended on its own layer.
    water = Image.new("RGBA", (W, H))
    wd = ImageDraw.Draw(water)
    for row, alpha in ((0.86, 0.08), (0.93, 0.05)):
        y0 = H * row
        wave = [(x, y0 - H * 0.012 * abs(math.sin(math.pi * x / (W / 4.5)))) for x in range(-8, W + 9, 2)]
        wd.line(wave, fill=(238, 240, 243, round(255 * alpha)), width=round(W * 0.016), joint="curve")
    img.alpha_composite(water)
    d = ImageDraw.Draw(img)
    k = W * 0.5 / 24
    hull(d, k, (W - 24 * k) / 2, H * 0.2, WHITE, AMBER)
    font = brand_font(round(W * 0.2))
    d.text((W / 2, H * 0.58), "Ferry", font=font, fill=WHITE, anchor="mm")
    small = brand_font(round(W * 0.068), "Regular")
    for i, line in enumerate(("Folders carried,", "not uploaded.")):
        d.text((W / 2, H * 0.67 + i * W * 0.09), line, font=small, fill=(195, 200, 205, 255), anchor="mm")
    return img.resize((ow, oh), Image.LANCZOS).convert("RGB")


def installer_header(scale=1.0, w=150, h=57):
    """NSIS inner-page header image (right-aligned on the white header band) at one display scale."""
    ow, oh = round(w * scale), round(h * scale)
    W, H = ow * 4, oh * 4
    img = Image.new("RGBA", (W, H), WHITE)
    k = H * 0.78 / 24
    hull(ImageDraw.Draw(img), k, W - 24 * k - H * 0.12, H * 0.08, oklch(0.2, 0.01, 250), AMBER_DEEP)
    return img.resize((ow, oh), Image.LANCZOS).convert("RGB")


def save_ico(path, images):
    images = sorted(images, key=lambda i: -i.width)
    images[0].save(path, sizes=[i.size for i in images], append_images=images[1:])


if __name__ == "__main__":
    # Every size Windows picks from at 100-200% scaling, so nothing is stretched up from a smaller image.
    save_ico(OUT / "ferry.ico", [app_icon(n) for n in (16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 128, 256)])
    app_icon(512).save(OUT / "ferry-512.png")
    for state in ("idle", "carrying", "attention", "paused"):
        for taskbar in ("light", "dark"):
            save_ico(OUT / f"tray-{state}-{taskbar}.ico", [tray_icon(n, state, taskbar) for n in (16, 20, 24, 32)])
    # Installer artwork (24-bit BMP, the format NSIS needs), one file per display scale; the
    # installer loads the one matching the screen's DPI so Windows never resizes it (that looks jagged).
    for old in OUT.glob("installer-*.bmp"):
        old.unlink()
    for pct in (100, 125, 150, 175, 200):
        installer_side(pct / 100).save(OUT / f"installer-side-{pct}.bmp")
        installer_header(pct / 100).save(OUT / f"installer-header-{pct}.bmp")

    # Self-check: the conversion matches known anchors, and every icon has the sizes Windows asks for.
    assert oklch(1, 0, 0)[:3] == (255, 255, 255) and oklch(0, 0, 0)[:3] == (0, 0, 0)
    assert Image.open(OUT / "ferry.ico").info["sizes"] >= {(16, 16), (32, 32), (256, 256)}
    assert Image.open(OUT / "tray-idle-dark.ico").info["sizes"] == {(16, 16), (20, 20), (24, 24), (32, 32)}
    print("ok")
