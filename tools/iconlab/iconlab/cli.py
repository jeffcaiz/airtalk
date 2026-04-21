"""Render the 空 glyph + koe-style pill waveform into airtalk.ico + tray RGBA.

Run via `uv run iconlab`. Outputs:
  tools/iconlab/out/airtalk.ico         (16/32/48/64/128/256, for .exe icon)
  tools/iconlab/out/tray_icon.rgba      (32x32 raw RGBA, for tray-icon crate)
  tools/iconlab/out/preview_<size>.png  (per-size previews for eyeballing)

Also copies airtalk.ico + tray_icon.rgba into `airtalk/assets/`.

Default font: 玄冬楷书 (Skr-ZERO/Xuandong-Kaishu, SIL OFL 1.1).
First run downloads into `tools/iconlab/fonts/`.

Composition (koe-inspired):
  1. White background with subtle vertical depth
  2. 5 pill-shaped vertical bars with blue→cyan vertical gradient,
     drawn BEHIND the glyph. Heights form a symmetric arch.
  3. 「空」glyph overlaid on top at low alpha, so the waveform shows
     through the glyph strokes.

Tune visuals by editing the CONFIG block below.
"""

from __future__ import annotations

import argparse
import shutil
import sys
import urllib.request
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

# ─── CONFIG (edit me) ──────────────────────────────────────────────────

CHAR = "空"

# Background — light.
BG_TOP = (255, 255, 255, 255)
BG_BOTTOM = (248, 251, 255, 255)     # tiny cool tint at bottom for depth

# Waveform pill gradient (koe-style). Top = darker royal blue,
# bottom = bright cyan. Feels lively under the soft glyph.
BAR_TOP_RGB = (18, 85, 180)
BAR_BOTTOM_RGB = (55, 190, 245)

# Glyph: drawn ON TOP of the waveform at low alpha. The waveform
# punches through wherever the glyph strokes cross it.
GLYPH_RGB = (18, 60, 140)            # deep blue, slightly darker than BAR_TOP
GLYPH_ALPHA = 0.32                   # 0..1; enough to read as a ghost
GLYPH_SCALE = 0.85
GLYPH_V_NUDGE = -0.02                # optical centering for CJK

CORNER_RATIO = 0.1875                # rounded-square corner, 48/256

# Waveform — 5 pill bars, symmetric arch (matches koe's reference icon).
WAVEFORM_ENABLED_MIN_SIZE = 48
WAVEFORM_HEIGHTS = [0.30, 0.62, 1.00, 0.62, 0.30]
WAVEFORM_BAR_WIDTH_RATIO = 1 / 11    # pill thickness
WAVEFORM_BAR_GAP_RATIO = 1 / 22
WAVEFORM_BAND_TOP = 0.16             # band spans most of the icon height
WAVEFORM_BAND_BOTTOM = 0.84

SIZES = [16, 32, 48, 64, 128, 256]

# Default font: 玄冬楷书 (Skr-ZERO/Xuandong-Kaishu), SIL OFL 1.1.
DEFAULT_FONT_URL = (
    "https://raw.githubusercontent.com/Skr-ZERO/Xuandong-Kaishu/main/"
    "%E7%8E%84%E5%86%AC%E6%A5%B7%E4%B9%A6.ttf"
)
DEFAULT_FONT_NAME = "XuandongKaishu.ttf"


# ─── Font resolution ───────────────────────────────────────────────────

def find_or_download_font(fonts_dir: Path) -> Path:
    fonts_dir.mkdir(parents=True, exist_ok=True)
    target = fonts_dir / DEFAULT_FONT_NAME
    if target.exists():
        return target
    print(f"No {DEFAULT_FONT_NAME} in {fonts_dir}; downloading…")
    print(f"  {DEFAULT_FONT_URL}")
    try:
        urllib.request.urlretrieve(DEFAULT_FONT_URL, target)
    except Exception as e:
        print(f"\nDownload failed: {e}", file=sys.stderr)
        print(
            "\nFallback: grab any CJK-capable OTF/TTF and save to "
            f"{target}, or pass --font <path>.",
            file=sys.stderr,
        )
        sys.exit(1)
    print(f"Saved to {target}")
    return target


# ─── Rendering ─────────────────────────────────────────────────────────

def gradient_rounded_square(size: int, radius: int) -> Image.Image:
    """RGBA icon base: vertical gradient, rounded-square mask."""
    bg = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(bg)
    for y in range(size):
        t = y / max(1, size - 1)
        col = tuple(int(BG_TOP[i] * (1 - t) + BG_BOTTOM[i] * t) for i in range(4))
        draw.line([(0, y), (size - 1, y)], fill=col)

    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (0, 0, size - 1, size - 1), radius=radius, fill=255
    )
    out = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    out.paste(bg, (0, 0), mask)
    return out


def draw_gradient_pill(
    img: Image.Image,
    cx: float,
    cy: float,
    width: float,
    height: float,
    top_rgb: tuple[int, int, int],
    bottom_rgb: tuple[int, int, int],
) -> None:
    """Pill-shaped (fully rounded) bar with a vertical gradient. `width`
    is the pill diameter; `height` clamps to at least `width` so the
    rounded caps don't overlap."""
    w = max(2, int(round(width)))
    h = max(w, int(round(height)))
    radius = w // 2

    canvas = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    d = ImageDraw.Draw(canvas)
    for y in range(h):
        t = y / max(1, h - 1)
        col = (
            int(top_rgb[0] * (1 - t) + bottom_rgb[0] * t),
            int(top_rgb[1] * (1 - t) + bottom_rgb[1] * t),
            int(top_rgb[2] * (1 - t) + bottom_rgb[2] * t),
            255,
        )
        d.line([(0, y), (w - 1, y)], fill=col)

    mask = Image.new("L", (w, h), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (0, 0, w - 1, h - 1), radius=radius, fill=255
    )
    canvas.putalpha(mask)

    img.alpha_composite(canvas, (int(round(cx - w / 2)), int(round(cy - h / 2))))


def draw_waveform(img: Image.Image) -> None:
    size = img.width
    if size < WAVEFORM_ENABLED_MIN_SIZE:
        return
    band_top = size * WAVEFORM_BAND_TOP
    band_bot = size * WAVEFORM_BAND_BOTTOM
    band_h = band_bot - band_top
    y_center = (band_top + band_bot) / 2

    bar_w = max(2, int(round(size * WAVEFORM_BAR_WIDTH_RATIO)))
    bar_gap = max(1, int(round(size * WAVEFORM_BAR_GAP_RATIO)))
    n_bars = len(WAVEFORM_HEIGHTS)
    total_w = n_bars * bar_w + (n_bars - 1) * bar_gap
    start_x = (size - total_w) / 2

    for i, h_norm in enumerate(WAVEFORM_HEIGHTS):
        h = max(bar_w, band_h * h_norm)
        cx = start_x + i * (bar_w + bar_gap) + bar_w / 2
        draw_gradient_pill(img, cx, y_center, bar_w, h, BAR_TOP_RGB, BAR_BOTTOM_RGB)


def draw_glyph_soft(img: Image.Image, size: int, font_path: Path) -> None:
    """Draw the glyph as a semi-transparent overlay via alpha-composite."""
    layer = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    glyph_size = max(6, int(round(size * GLYPH_SCALE)))
    font = ImageFont.truetype(str(font_path), glyph_size)
    draw = ImageDraw.Draw(layer)
    cx = size / 2
    cy = size / 2 + size * GLYPH_V_NUDGE
    draw.text(
        (cx, cy),
        CHAR,
        font=font,
        fill=(GLYPH_RGB[0], GLYPH_RGB[1], GLYPH_RGB[2], 255),
        anchor="mm",
    )
    r, g, b, a = layer.split()
    a = a.point(lambda v: int(v * GLYPH_ALPHA))
    layer = Image.merge("RGBA", (r, g, b, a))
    img.alpha_composite(layer)


def render_icon(size: int, font_path: Path) -> Image.Image:
    radius = max(1, int(round(size * CORNER_RATIO)))
    img = gradient_rounded_square(size, radius)
    draw_waveform(img)
    draw_glyph_soft(img, size, font_path)
    return img


# ─── Output ────────────────────────────────────────────────────────────

def save_ico(icons: list[Image.Image], path: Path) -> None:
    sizes = [(img.width, img.height) for img in icons]
    largest = max(icons, key=lambda i: i.width)
    largest.save(str(path), format="ICO", sizes=sizes)


def save_tray_rgba(icon_32: Image.Image, path: Path) -> None:
    with open(path, "wb") as f:
        f.write(icon_32.convert("RGBA").tobytes())


# ─── Entry ─────────────────────────────────────────────────────────────

def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--no-copy", action="store_true")
    parser.add_argument("--font", type=Path, help="Override default font path")
    args = parser.parse_args()

    iconlab_root = Path(__file__).resolve().parents[1]
    airtalk_root = iconlab_root.parents[1]
    fonts_dir = iconlab_root / "fonts"
    out_dir = iconlab_root / "out"
    out_dir.mkdir(parents=True, exist_ok=True)

    font_path = args.font if args.font else find_or_download_font(fonts_dir)
    print(f"Font: {font_path.name}")

    icons = [render_icon(s, font_path) for s in SIZES]

    ico_path = out_dir / "airtalk.ico"
    save_ico(icons, ico_path)
    print(f"wrote {ico_path.relative_to(iconlab_root)}")

    icon_32 = next(i for i in icons if i.width == 32)
    tray_path = out_dir / "tray_icon.rgba"
    save_tray_rgba(icon_32, tray_path)
    print(f"wrote {tray_path.relative_to(iconlab_root)}")

    for img in icons:
        p = out_dir / f"preview_{img.width}.png"
        img.save(p)
    print(f"wrote preview_*.png ({len(icons)} sizes)")

    if not args.no_copy:
        assets = airtalk_root / "airtalk" / "assets"
        assets.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ico_path, assets / "airtalk.ico")
        shutil.copy2(tray_path, assets / "tray_icon.rgba")
        print(f"copied into {assets.relative_to(airtalk_root)}/")


if __name__ == "__main__":
    main()
