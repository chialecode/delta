"""Rasterize the DELTA marks into multi-size .ico files.

`icon.ico` mirrors crates/delta-desktop/assets/icon.svg (release: a white "D" on a dark gray
rounded square). `icon-dev.ico` mirrors icon-dev.svg (development identities: the same mark
with an amber "DEV" badge in the bottom-right corner). Both SVGs use a 1024-unit space, as
here. The mark is rendered once at 1024 x 1024 and every size is averaged down from that
render. Standard library only and deterministic: the same script always produces the same
bytes. Run from the repository root:  python scripts/generate_icon.py
"""

from __future__ import annotations

import struct
import zlib
from pathlib import Path

SIZES = (16, 24, 32, 48, 64, 128, 256)
BACKGROUND = (0x3C, 0x40, 0x43)
LETTER = (0xFF, 0xFF, 0xFF)
BADGE = (0xF3, 0xA1, 0x15)
BADGE_TEXT = (0x20, 0x21, 0x24)
SUPERSAMPLE = 2
UNITS = 1024.0


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


def render_master(sample) -> list:
    """Render the mark once at 1024 x 1024 (2 x 2 samples a pixel) as RGBA rows."""
    master = int(UNITS)
    rows = []
    for py in range(master):
        row = []
        for px in range(master):
            totals, covered = [0, 0, 0], 0
            for sy in range(SUPERSAMPLE):
                for sx in range(SUPERSAMPLE):
                    colour = sample(px + (sx + 0.5) / SUPERSAMPLE, py + (sy + 0.5) / SUPERSAMPLE)
                    if colour is not None:
                        covered += 1
                        for i in range(3):
                            totals[i] += colour[i]
            if covered:
                row.append((totals[0] / covered, totals[1] / covered, totals[2] / covered,
                            255 * covered / SUPERSAMPLE ** 2))
            else:
                row.append((0, 0, 0, 0))
        rows.append(row)
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


def write_ico(name: str, sample) -> None:
    master = render_master(sample)
    images = [encode_png(average_down(master, size)) for size in SIZES]
    offset = 6 + 16 * len(images)
    directory = struct.pack("<HHH", 0, 1, len(images))
    for size, image in zip(SIZES, images):
        dimension = 0 if size == 256 else size
        directory += struct.pack("<BBBBHHII", dimension, dimension, 0, 0, 1, 32, len(image), offset)
        offset += len(image)
    target = Path(__file__).resolve().parents[1] / "crates/delta-desktop/assets" / name
    target.write_bytes(directory + b"".join(images))
    print(f"wrote {target.name}: {', '.join(map(str, SIZES))} px")


def main() -> None:
    write_ico("icon.ico", sample)
    write_ico("icon-dev.ico", sample_dev)


if __name__ == "__main__":
    main()
