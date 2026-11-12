"""Rasterize the DELTA mark (crates/delta-desktop/assets/icon.svg) into a multi-size .ico.

Standard library only and deterministic: the same script always produces the same bytes.
The geometry mirrors icon.svg (rounded square + even-odd outlined triangle). Run from the
repository root:  python scripts/generate_icon.py
"""

from __future__ import annotations

import struct
import zlib
from pathlib import Path

SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
BACKGROUND = (0x1F, 0x23, 0x29)
MARK = (0x14, 0xB8, 0xA6)
SUPERSAMPLE = 4


def inside_rounded_square(x: float, y: float) -> bool:
    left, top, right, bottom, radius = 8.0, 8.0, 248.0, 248.0, 52.0
    if not (left <= x <= right and top <= y <= bottom):
        return False
    cx = min(max(x, left + radius), right - radius)
    cy = min(max(y, top + radius), bottom - radius)
    return (x - cx) ** 2 + (y - cy) ** 2 <= radius ** 2


def inside_triangle(x: float, y: float, a, b, c) -> bool:
    def side(p, q):
        return (x - q[0]) * (p[1] - q[1]) - (p[0] - q[0]) * (y - q[1])

    d1, d2, d3 = side(a, b), side(b, c), side(c, a)
    negative = d1 < 0 or d2 < 0 or d3 < 0
    positive = d1 > 0 or d2 > 0 or d3 > 0
    return not (negative and positive)


def sample(x: float, y: float):
    if not inside_rounded_square(x, y):
        return None
    outer = inside_triangle(x, y, (128, 44), (222, 212), (34, 212))
    inner = inside_triangle(x, y, (128, 100), (176, 186), (80, 186))
    return MARK if outer and not inner else BACKGROUND


def render(size: int) -> bytes:
    rows = []
    scale = 256.0 / size
    for py in range(size):
        row = bytearray([0])  # PNG filter type: none
        for px in range(size):
            totals, covered = [0, 0, 0], 0
            for sy in range(SUPERSAMPLE):
                for sx in range(SUPERSAMPLE):
                    colour = sample((px + (sx + 0.5) / SUPERSAMPLE) * scale,
                                    (py + (sy + 0.5) / SUPERSAMPLE) * scale)
                    if colour is not None:
                        covered += 1
                        for i in range(3):
                            totals[i] += colour[i]
            if covered:
                row.extend(round(t / covered) for t in totals)
                row.append(round(255 * covered / SUPERSAMPLE ** 2))
            else:
                row.extend((0, 0, 0, 0))
        rows.append(bytes(row))

    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
            + chunk(b"IDAT", zlib.compress(b"".join(rows), 9)) + chunk(b"IEND", b""))


def main() -> None:
    images = [render(size) for size in SIZES]
    offset = 6 + 16 * len(images)
    directory = struct.pack("<HHH", 0, 1, len(images))
    for size, image in zip(SIZES, images):
        dimension = 0 if size == 256 else size
        directory += struct.pack("<BBBBHHII", dimension, dimension, 0, 0, 1, 32, len(image), offset)
        offset += len(image)
    target = Path(__file__).resolve().parents[1] / "crates/delta-desktop/assets/icon.ico"
    target.write_bytes(directory + b"".join(images))
    print(f"wrote {target.name}: {', '.join(map(str, SIZES))} px")


if __name__ == "__main__":
    main()
