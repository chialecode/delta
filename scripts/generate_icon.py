"""Generate or verify the DELTA SVG and multi-size .ico brand marks.

`icon.ico` mirrors crates/delta-desktop/assets/icon.svg (release: a white "D" on a dark gray
rounded square). `icon-dev.ico` mirrors icon-dev.svg (development identities: the same mark
with an amber "DEV" badge in the bottom-right corner). Both SVGs use a 1024-unit space, as
here. The mark is rendered once at 1024 x 1024 and every size is averaged down from that
render. Standard library only and deterministic: the same script always produces the same
bytes. Run from the repository root:  python scripts/generate_icon.py [--check]
"""

from __future__ import annotations

import argparse
import struct
import sys
import zlib
from pathlib import Path

ASSETS_DIR = Path(__file__).resolve().parents[1] / "crates/delta-desktop/assets"
SIZES = (16, 24, 32, 48, 64, 128, 256)
BACKGROUND = (0x3C, 0x40, 0x43)
LETTER = (0xFF, 0xFF, 0xFF)
BADGE = (0xF3, 0xA1, 0x15)
BADGE_TEXT = (0x20, 0x21, 0x24)
SUPERSAMPLE = 2
UNITS = 1024.0


def hex_colour(colour: tuple[int, int, int]) -> str:
    return f"#{colour[0]:02X}{colour[1]:02X}{colour[2]:02X}"


def render_svg(dev: bool = False) -> bytes:
    bg = hex_colour(BACKGROUND)
    fg = hex_colour(LETTER)
    lines = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024" width="256" height="256">',
        (
            '  <!-- DELTA Dev mark: the release mark with an amber "DEV" badge '
            '(development identities). Rasterized by scripts/generate_icon.py. -->'
            if dev
            else '  <!-- DELTA application mark (S-28): a white "D" on dark gray '
            '(letter height 250-780, stroke 110). Rasterized by scripts/generate_icon.py. -->'
        ),
        f'  <rect x="48" y="48" width="928" height="928" rx="200" fill="{bg}"/>',
        (
            '  <path d="M320 250H467A265 265 0 0 1 467 780H320A28 28 0 0 1 292 752V278'
            f'A28 28 0 0 1 320 250ZM402 360V670H467A155 155 0 0 0 467 360Z" fill="{fg}" '
            'fill-rule="evenodd"/>'
        ),
    ]
    if dev:
        badge = hex_colour(BADGE)
        text = hex_colour(BADGE_TEXT)
        lines.extend(
            [
                f'  <rect x="560" y="800" width="400" height="160" rx="52" fill="{badge}"/>',
                (
                    f'  <path fill="{text}" fill-rule="evenodd" d="M636 839H670A40 40 0 0 1 '
                    '710 879V885A40 40 0 0 1 670 925H636ZM654 857V907H670A22 22 0 0 0 692 885'
                    'V879A22 22 0 0 0 670 857ZM726 839H782V857H744V873H776V891H744V907H782V925'
                    'H726ZM796 839H815L840 903L865 839H884L850 925H830Z"/>'
                ),
            ]
        )
    lines.append("</svg>")
    return ("\n".join(lines) + "\n").encode("utf-8")


def inside_rounded_rect(x: float, y: float, left: float, top: float, right: float,
                        bottom: float, radius: float) -> bool:
    if not (left <= x <= right and top <= y <= bottom):
        return False
    cx = min(max(x, left + radius), right - radius)
    cy = min(max(y, top + radius), bottom - radius)
    return (x - cx) ** 2 + (y - cy) ** 2 <= radius ** 2


def inside_right_rounded(x: float, y: float, left: float, top: float, right: float,
                         bottom: float, radius: float) -> bool:
    """A rectangle whose two right-hand corners are rounded (the bowl of the badge's D)."""
    if not (left <= x <= right and top <= y <= bottom):
        return False
    cx = right - radius
    if x <= cx:
        return True
    cy = min(max(y, top + radius), bottom - radius)
    return (x - cx) ** 2 + (y - cy) ** 2 <= radius ** 2


def inside_polygon(x: float, y: float, points) -> bool:
    inside = False
    for (x1, y1), (x2, y2) in zip(points, points[1:] + points[:1]):
        if (y1 > y) != (y2 > y) and x < x1 + (y - y1) * (x2 - x1) / (y2 - y1):
            inside = not inside
    return inside


def inside_letter_d(x: float, y: float) -> bool:
    """The mark's "D": height 250-780, stroke 110, a half-round bowl about (467, 515)."""
    if x <= 467:
        outer = inside_rounded_rect(x, y, 292, 250, 467 + 28, 780, 28) and x <= 467
    else:
        outer = (x - 467) ** 2 + (y - 515) ** 2 <= 265 ** 2
    if x <= 467:
        inner = 402 <= x and 360 <= y <= 670
    else:
        inner = (x - 467) ** 2 + (y - 515) ** 2 <= 155 ** 2
    return outer and not inner


V_LETTER = [(796, 839), (815, 839), (840, 903), (865, 839), (884, 839), (850, 925), (830, 925)]


def inside_badge_text(x: float, y: float) -> bool:
    d = (inside_right_rounded(x, y, 636, 839, 710, 925, 40)
         and not inside_right_rounded(x, y, 654, 857, 692, 907, 22))
    e = (726 <= x <= 744 and 839 <= y <= 925) or any(
        726 <= x <= right and top <= y <= bottom
        for right, top, bottom in ((782, 839, 857), (776, 873, 891), (782, 907, 925)))
    return d or e or inside_polygon(x, y, V_LETTER)


def sample(x: float, y: float):
    if not inside_rounded_rect(x, y, 48, 48, 976, 976, 200):
        return None
    return LETTER if inside_letter_d(x, y) else BACKGROUND


def sample_dev(x: float, y: float):
    if inside_rounded_rect(x, y, 560, 800, 960, 960, 52):
        return BADGE_TEXT if inside_badge_text(x, y) else BADGE
    return sample(x, y)


def _sample_pixel(sample_fn, px: int, py: int) -> tuple[float, float, float, float]:
    totals, covered = [0, 0, 0], 0
    for sy in range(SUPERSAMPLE):
        for sx in range(SUPERSAMPLE):
            colour = sample_fn(px + (sx + 0.5) / SUPERSAMPLE, py + (sy + 0.5) / SUPERSAMPLE)
            if colour is not None:
                covered += 1
                for i in range(3):
                    totals[i] += colour[i]
    if covered:
        return (
            totals[0] / covered,
            totals[1] / covered,
            totals[2] / covered,
            255 * covered / SUPERSAMPLE ** 2,
        )
    return (0, 0, 0, 0)


def render_master(sample_fn) -> list:
    """Render the mark once at 1024 x 1024 (2 x 2 samples a pixel) as RGBA rows."""
    master = int(UNITS)
    return [[_sample_pixel(sample_fn, px, py) for px in range(master)] for py in range(master)]


def render_dev_master(release_master: list) -> list:
    """Overlay the bottom-right DEV badge onto an already-rendered release master."""
    rows = list(release_master)
    for py in range(799, 962):
        base_row = release_master[py]
        patched = list(base_row)
        for px in range(559, 962):
            patched[px] = _sample_pixel(sample_dev, px, py)
        rows[py] = patched
    return rows


def average_down(master: list, size: int) -> list:
    """Average the area each pixel covers (alpha-weighted, so the transparent corners do not
    darken the edges); every size is drawn the same way, so the strokes keep their weight."""
    scale = len(master) / size
    out = []
    for y in range(size):
        y0 = int(y * scale)
        y1 = max(int((y + 1) * scale), y0 + 1)
        row = []
        for x in range(size):
            x0 = int(x * scale)
            x1 = max(int((x + 1) * scale), x0 + 1)
            r = g = b = a = 0.0
            for line in master[y0:y1]:
                for pr, pg, pb, pa in line[x0:x1]:
                    r, g, b, a = r + pr * pa, g + pg * pa, b + pb * pa, a + pa
            count = (y1 - y0) * (x1 - x0)
            if a:
                row.append((round(r / a), round(g / a), round(b / a), round(a / count)))
            else:
                row.append((0, 0, 0, 0))
        out.append(row)
    return out


def encode_png(pixels: list) -> bytes:
    size = len(pixels)
    raw = b"".join(bytes([0]) + bytes(v for px in row for v in px) for row in pixels)

    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def encode_ico(master: list) -> bytes:
    images = [encode_png(average_down(master, size)) for size in SIZES]
    offset = 6 + 16 * len(images)
    directory = struct.pack("<HHH", 0, 1, len(images))
    for size, image in zip(SIZES, images):
        dimension = 0 if size == 256 else size
        directory += struct.pack("<BBBBHHII", dimension, dimension, 0, 0, 1, 32, len(image), offset)
        offset += len(image)
    return directory + b"".join(images)


def render_ico(sample_fn) -> bytes:
    return encode_ico(render_master(sample_fn))


def expected_assets() -> dict[str, bytes]:
    release_master = render_master(sample)
    dev_master = render_dev_master(release_master)
    return {
        "icon.svg": render_svg(dev=False),
        "icon-dev.svg": render_svg(dev=True),
        "icon.ico": encode_ico(release_master),
        "icon-dev.ico": encode_ico(dev_master),
    }


def write_ico(name: str, sample_fn, assets_dir: Path = ASSETS_DIR) -> None:
    target = assets_dir / name
    target.write_bytes(render_ico(sample_fn))
    print(f"wrote {target.name}: {', '.join(map(str, SIZES))} px")


def write_assets(assets_dir: Path = ASSETS_DIR, expected: dict[str, bytes] | None = None) -> None:
    assets_dir.mkdir(parents=True, exist_ok=True)
    rendered = expected if expected is not None else expected_assets()
    for name, payload in rendered.items():
        target = assets_dir / name
        target.write_bytes(payload)
        if name.endswith(".ico"):
            print(f"wrote {target.name}: {', '.join(map(str, SIZES))} px")
        else:
            print(f"wrote {target.name}: {len(payload)} B")


def check_assets(
    assets_dir: Path = ASSETS_DIR,
    expected: dict[str, bytes] | None = None,
) -> list[str]:
    rendered = expected if expected is not None else expected_assets()
    errors: list[str] = []
    for name, payload in rendered.items():
        target = assets_dir / name
        if not target.is_file():
            errors.append(f"missing {name}")
            continue
        actual = target.read_bytes()
        if actual != payload:
            errors.append(
                f"{name} differs from scripts/generate_icon.py output "
                f"(actual {len(actual)} B, expected {len(payload)} B)"
            )
    return errors


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--check",
        action="store_true",
        help="verify committed .svg and .ico files match generator output byte-for-byte",
    )
    parser.add_argument(
        "--assets-dir",
        type=Path,
        default=ASSETS_DIR,
        help="directory containing icon.svg, icon-dev.svg, icon.ico, and icon-dev.ico",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    if args.check:
        errors = check_assets(args.assets_dir)
        if errors:
            for err in errors:
                print(f"FAIL: {err}", file=sys.stderr)
            return 1
        print(f"PASS: {', '.join(('icon.svg', 'icon-dev.svg', 'icon.ico', 'icon-dev.ico'))} match generator output.")
        return 0
    write_assets(args.assets_dir)
    return 0


if __name__ == "__main__":
    sys.exit(main())
