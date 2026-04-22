"""Build Airtalk icons from a simple Lucide mic mark.

Run via `uv run iconlab`. Outputs:
  tools/iconlab/out/icon.svg           reference SVG (design source of truth)
  tools/iconlab/out/icon-<size>.png    rendered PNG per size
  tools/iconlab/out/airtalk.ico        multi-size ICO for Windows
  tools/iconlab/out/tray_icon.rgba     32x32 raw RGBA for tray-icon

Also copies `airtalk.ico` + `tray_icon.rgba` into `airtalk/assets/`.

Rendering is pure Pillow: one master at MASTER_SIZE, then LANCZOS downscale for
each target. The mic glyph is the Lucide "mic" path reconstructed with
primitives (rounded-rect stadium + arc + lines + round caps).
"""

from __future__ import annotations

import argparse
import shutil
from pathlib import Path

from PIL import Image, ImageDraw

BG_COLOR = "#f97316"
ICON_COLOR = "#ffffff"
VIEWBOX_SIZE = 512
CANVAS_RADIUS = 115
ICON_OFFSET = 112
ICON_UNIT = 12
STROKE_UNITS = 2

SIZES = [16, 24, 32, 48, 64, 96, 128, 192, 256, 512]
ICO_SIZES = [16, 32, 48, 64, 128, 256]
MASTER_SIZE = 2048


def build_svg(size: int = VIEWBOX_SIZE) -> str:
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" '
        f'width="{size}" height="{size}" viewBox="0 0 {VIEWBOX_SIZE} {VIEWBOX_SIZE}">'
        f'<rect x="0" y="0" width="{VIEWBOX_SIZE}" height="{VIEWBOX_SIZE}" '
        f'rx="{CANVAS_RADIUS}" fill="{BG_COLOR}"/>'
        f'<g transform="translate({ICON_OFFSET}, {ICON_OFFSET}) scale({ICON_UNIT})" '
        f'fill="none" stroke="{ICON_COLOR}" stroke-width="{STROKE_UNITS}" '
        'stroke-linecap="round" stroke-linejoin="round">'
        '<path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z"/>'
        '<path d="M19 10v2a7 7 0 0 1-14 0v-2"/>'
        '<line x1="12" y1="19" x2="12" y2="22"/>'
        "</g>"
        "</svg>"
    )


def render_master(master_size: int = MASTER_SIZE) -> Image.Image:
    """Render the icon at master_size x master_size using Pillow primitives.

    Geometry is expressed in Lucide's 24-unit coord space, mapped through the
    512-unit viewbox into master pixels. The mic capsule path in Lucide's mic
    glyph is actually 6 wide x 13 tall (y=2..15) — the bottom semicircle
    bulges down past the straight sides' end at y=12.
    """
    scale = master_size / VIEWBOX_SIZE
    lucide = ICON_UNIT * scale                  # 1 Lucide unit in master px
    stroke_px = STROKE_UNITS * lucide
    half_stroke = stroke_px / 2.0

    def gx(x: float) -> float:
        return (ICON_OFFSET + x * ICON_UNIT) * scale

    def gy(y: float) -> float:
        return (ICON_OFFSET + y * ICON_UNIT) * scale

    image = Image.new("RGBA", (master_size, master_size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)

    # Cyan rounded-square background.
    draw.rounded_rectangle(
        (0, 0, master_size - 1, master_size - 1),
        radius=CANVAS_RADIUS * scale,
        fill=BG_COLOR,
    )

    stroke_w = max(1, int(round(stroke_px)))

    # U-arc: bottom half of circle at (12, 12) r=7. Angles 0..180 (east -> west
    # through south, since SVG/Pillow y-down makes 90 degrees visually south).
    # Pillow's arc with width > 1 strokes INWARD from the bbox's inscribed
    # ellipse, not centered on it. To get an SVG-centered stroke around nominal
    # radius r, grow the bbox radius to r + half_stroke so the inward-stroked
    # band [r - half_stroke, r + half_stroke] matches the SVG's centered stroke.
    cx, cy = gx(12), gy(12)
    arc_outer = 7 * lucide + half_stroke
    draw.arc(
        (cx - arc_outer, cy - arc_outer, cx + arc_outer, cy + arc_outer),
        start=0, end=180,
        fill=ICON_COLOR,
        width=stroke_w,
    )

    def round_cap(x: float, y: float) -> None:
        draw.ellipse(
            (x - half_stroke, y - half_stroke, x + half_stroke, y + half_stroke),
            fill=ICON_COLOR,
        )

    def rcline(p0: tuple[float, float], p1: tuple[float, float]) -> None:
        # Thick line + filled circles at each end simulate stroke-linecap="round"
        # and stroke-linejoin="round" (where the stubs meet the arc).
        draw.line([p0, p1], fill=ICON_COLOR, width=stroke_w)
        round_cap(*p0)
        round_cap(*p1)

    # Stubs from arc ends up to y=10.
    rcline((gx(5), gy(10)), (gx(5), gy(12)))
    rcline((gx(19), gy(10)), (gx(19), gy(12)))

    # Stem below the U.
    rcline((gx(12), gy(19)), (gx(12), gy(22)))

    # Mic capsule drawn last as outer(white) + inner(bg). Pillow's outline=width
    # strokes inward, which would shrink the capsule; the two-fill sandwich
    # gives a centered stroke that matches the SVG stroke-width=2.
    draw.rounded_rectangle(
        (gx(9) - half_stroke, gy(2) - half_stroke,
         gx(15) + half_stroke, gy(15) + half_stroke),
        radius=3 * lucide + half_stroke,
        fill=ICON_COLOR,
    )
    draw.rounded_rectangle(
        (gx(9) + half_stroke, gy(2) + half_stroke,
         gx(15) - half_stroke, gy(15) - half_stroke),
        radius=3 * lucide - half_stroke,
        fill=BG_COLOR,
    )

    return image


def save_ico(images: list[Image.Image], path: Path) -> None:
    primary = max(images, key=lambda image: image.width)
    primary.save(
        path,
        format="ICO",
        sizes=[(image.width, image.height) for image in images],
        append_images=[image for image in images if image is not primary],
    )


def save_tray_rgba(icon_32: Image.Image, path: Path) -> None:
    with open(path, "wb") as file:
        file.write(icon_32.convert("RGBA").tobytes())


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--no-copy", action="store_true")
    args = parser.parse_args()

    iconlab_root = Path(__file__).resolve().parents[1]
    airtalk_root = iconlab_root.parents[1]
    out_dir = iconlab_root / "out"
    out_dir.mkdir(parents=True, exist_ok=True)

    (out_dir / "icon.svg").write_text(build_svg(), encoding="utf-8")

    master = render_master()

    rendered: dict[int, Image.Image] = {}
    for size in SIZES:
        image = master.resize((size, size), Image.Resampling.LANCZOS)
        image.save(out_dir / f"icon-{size}.png")
        rendered[size] = image
        print(f"  {size:>4}x{size:<4}")

    ico_path = out_dir / "airtalk.ico"
    save_ico([rendered[size] for size in ICO_SIZES], ico_path)
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
