"""Build Airtalk icons from a 16-unit SVG design.

Run via `uv run iconlab`. Outputs:
  tools/iconlab/out/icon-<size>.svg    source SVG per size
  tools/iconlab/out/icon-<size>.png    rendered PNG per size
  tools/iconlab/out/airtalk.ico        multi-size ICO for Windows
  tools/iconlab/out/tray_icon.rgba     32x32 raw RGBA for tray-icon

Also copies `airtalk.ico` + `tray_icon.rgba` into `airtalk/assets/`.
"""

from __future__ import annotations

import argparse
import base64
import shutil
import urllib.request
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFont

# Design tokens in a 16-unit viewBox.
MASK_COLOR = "#0f172a"
MASK_OPACITY = 0.85
PAPER_COLOR = "#fbf6e4"
KONG_COLOR = "#a8a29e"

CORNER_RX = 3

KONG_MIN_SIZE = 48
KONG_FONT_SIZE = 12
FONT_FAMILY = "Zhi Mang Xing"
DEFAULT_FONT_NAME = "ZhiMangXing-Regular.ttf"
DEFAULT_FONT_URL = (
    "https://raw.githubusercontent.com/google/fonts/main/"
    "ofl/zhimangxing/ZhiMangXing-Regular.ttf"
)

GRADIENT_SOLID_END = 50
GRADIENT_FADE_END = 100

WAVE_BARS = [
    (1, 6, 2, 4),
    (4, 3, 2, 10),
    (7, 1, 2, 14),
    (10, 3, 2, 10),
    (13, 6, 2, 4),
]

SIZES = [16, 24, 32, 48, 64, 96, 128, 192, 256, 512]
ICO_SIZES = [16, 32, 48, 64, 128, 256]
SUPERSAMPLE = 4


def ensure_font(fonts_dir: Path, override: Path | None) -> Path:
    if override is not None:
        if not override.exists():
            raise FileNotFoundError(f"font not found: {override}")
        return override

    fonts_dir.mkdir(parents=True, exist_ok=True)
    target = fonts_dir / DEFAULT_FONT_NAME
    if target.exists():
        return target

    print(f"missing {DEFAULT_FONT_NAME}; downloading")
    print(f"  {DEFAULT_FONT_URL}")
    urllib.request.urlretrieve(DEFAULT_FONT_URL, target)
    return target


def build_svg(size: int, font_path: Path) -> str:
    include_kong = size >= KONG_MIN_SIZE

    font_face = ""
    if include_kong:
        b64 = base64.b64encode(font_path.read_bytes()).decode("ascii")
        font_face = (
            "<style>@font-face{"
            "font-family:'ZMX';"
            f"src:url('data:font/ttf;base64,{b64}') format('truetype');"
            "}</style>"
        )

    font_stack = f"'ZMX','{FONT_FAMILY}','STXingkai','STKaiti','BiauKai',serif"

    kong_grad = ""
    kong_text = ""
    if include_kong:
        kong_grad = (
            '<linearGradient id="g" gradientUnits="userSpaceOnUse" '
            'x1="3" y1="3" x2="13" y2="13">'
            f'<stop offset="0%" stop-color="{KONG_COLOR}" stop-opacity="1"/>'
            f'<stop offset="{GRADIENT_SOLID_END}%" '
            f'stop-color="{KONG_COLOR}" stop-opacity="1"/>'
            f'<stop offset="{GRADIENT_FADE_END}%" '
            f'stop-color="{KONG_COLOR}" stop-opacity="0"/>'
            "</linearGradient>"
        )
        kong_text = (
            f'<text x="8" y="8" text-anchor="middle" dominant-baseline="central" '
            f'font-family="{font_stack}" font-size="{KONG_FONT_SIZE}" '
            'fill="url(#g)">空</text>'
        )

    bars = "".join(
        f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="1" fill="black"/>'
        for x, y, w, h in WAVE_BARS
    )

    return (
        '<svg xmlns="http://www.w3.org/2000/svg" '
        f'width="{size}" height="{size}" viewBox="0 0 16 16">'
        f"{font_face}"
        "<defs>"
        f'<clipPath id="c"><rect width="16" height="16" rx="{CORNER_RX}"/></clipPath>'
        f'<mask id="m"><rect width="16" height="16" fill="white"/>{bars}</mask>'
        f"{kong_grad}"
        "</defs>"
        '<g clip-path="url(#c)">'
        f'<rect width="16" height="16" fill="{PAPER_COLOR}"/>'
        f"{kong_text}"
        f'<rect width="16" height="16" fill="{MASK_COLOR}" '
        f'fill-opacity="{MASK_OPACITY}" mask="url(#m)"/>'
        "</g>"
        "</svg>"
    )


def hex_to_rgb(color: str) -> tuple[int, int, int]:
    color = color.lstrip("#")
    return tuple(int(color[index:index + 2], 16) for index in (0, 2, 4))


def render_png(size: int, font_path: Path) -> Image.Image:
    work_size = size * SUPERSAMPLE
    scale = work_size / 16.0
    include_kong = size >= KONG_MIN_SIZE

    paper = Image.new("RGBA", (work_size, work_size), (0, 0, 0, 0))
    paper_mask = Image.new("L", (work_size, work_size), 0)
    paper_draw = ImageDraw.Draw(paper_mask)
    radius = round(CORNER_RX * scale)
    paper_draw.rounded_rectangle(
        (0, 0, work_size - 1, work_size - 1),
        radius=radius,
        fill=255,
    )
    paper_color = (*hex_to_rgb(PAPER_COLOR), 255)
    paper.paste(Image.new("RGBA", (work_size, work_size), paper_color), (0, 0), paper_mask)

    if include_kong:
        paper.alpha_composite(render_kong_layer(work_size, scale, font_path))

    front = Image.new("RGBA", (work_size, work_size), (0, 0, 0, 0))
    front_mask = paper_mask.copy()
    front_draw = ImageDraw.Draw(front_mask)
    for x, y, w, h in WAVE_BARS:
        front_draw.rounded_rectangle(
            (
                round(x * scale),
                round(y * scale),
                round((x + w) * scale),
                round((y + h) * scale),
            ),
            radius=max(1, round(scale)),
            fill=0,
        )

    front_color = (*hex_to_rgb(MASK_COLOR), round(MASK_OPACITY * 255))
    front.paste(Image.new("RGBA", (work_size, work_size), front_color), (0, 0), front_mask)

    composited = Image.alpha_composite(paper, front)
    return composited.resize((size, size), Image.Resampling.LANCZOS)


def render_kong_layer(work_size: int, scale: float, font_path: Path) -> Image.Image:
    font_size = max(1, round(KONG_FONT_SIZE * scale))
    font = ImageFont.truetype(str(font_path), font_size)

    text_mask = Image.new("L", (work_size, work_size), 0)
    text_draw = ImageDraw.Draw(text_mask)
    text_draw.text(
        (work_size / 2, work_size / 2),
        "空",
        font=font,
        fill=255,
        anchor="mm",
    )

    gradient_alpha = Image.new("L", (work_size, work_size), 0)
    pixels = gradient_alpha.load()
    x1 = 3 * scale
    y1 = 3 * scale
    x2 = 13 * scale
    y2 = 13 * scale
    dx = x2 - x1
    dy = y2 - y1
    denom = dx * dx + dy * dy
    solid = GRADIENT_SOLID_END / 100.0
    fade = GRADIENT_FADE_END / 100.0

    for y in range(work_size):
        for x in range(work_size):
            proj = ((x - x1) * dx + (y - y1) * dy) / denom
            t = max(0.0, min(1.0, proj))
            if t <= solid:
                alpha = 255
            elif t >= fade:
                alpha = 0
            else:
                alpha = round(255 * (1.0 - (t - solid) / (fade - solid)))
            pixels[x, y] = alpha

    alpha_mask = ImageChops.multiply(text_mask, gradient_alpha)
    layer = Image.new("RGBA", (work_size, work_size), (*hex_to_rgb(KONG_COLOR), 0))
    layer.putalpha(alpha_mask)
    return layer


def save_ico(images: list[Image.Image], path: Path) -> None:
    ico_images = [image for image in images if image.width in ICO_SIZES]
    if not ico_images:
        raise ValueError("no icon sizes available for ICO output")

    primary = max(ico_images, key=lambda image: image.width)
    primary.save(
        path,
        format="ICO",
        sizes=[(image.width, image.height) for image in ico_images],
        append_images=[image for image in ico_images if image is not primary],
    )


def save_tray_rgba(icon_32: Image.Image, path: Path) -> None:
    with open(path, "wb") as file:
        file.write(icon_32.convert("RGBA").tobytes())


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--no-copy", action="store_true")
    parser.add_argument("--font", type=Path, help="Path to ZhiMangXing-Regular.ttf")
    args = parser.parse_args()

    iconlab_root = Path(__file__).resolve().parents[1]
    airtalk_root = iconlab_root.parents[1]
    fonts_dir = iconlab_root / "fonts"
    out_dir = iconlab_root / "out"
    out_dir.mkdir(parents=True, exist_ok=True)

    font_path = ensure_font(fonts_dir, args.font)
    print(f"font: {font_path}")

    rendered: dict[int, Image.Image] = {}
    for size in SIZES:
        svg_text = build_svg(size, font_path)
        svg_path = out_dir / f"icon-{size}.svg"
        png_path = out_dir / f"icon-{size}.png"
        svg_path.write_text(svg_text, encoding="utf-8")
        image = render_png(size, font_path)
        image.save(png_path)
        rendered[size] = image
        tag = "w/ 空" if size >= KONG_MIN_SIZE else "plain"
        print(f"  {size:>4}x{size:<4} [{tag}]")

    ico_path = out_dir / "airtalk.ico"
    save_ico([rendered[size] for size in SIZES], ico_path)
    print(f"wrote {ico_path.relative_to(iconlab_root)}")

    tray_path = out_dir / "tray_icon.rgba"
    save_tray_rgba(rendered[32], tray_path)
    print(f"wrote {tray_path.relative_to(iconlab_root)}")

    if not args.no_copy:
        assets = airtalk_root / "airtalk" / "assets"
        assets.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ico_path, assets / "airtalk.ico")
        shutil.copy2(tray_path, assets / "tray_icon.rgba")
        print(f"copied into {assets.relative_to(airtalk_root)}/")


if __name__ == "__main__":
    main()
