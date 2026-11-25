"""Fetch the pinned NSIS build that `delta-cli package-build` compiles installers with (S-25).

Standard library only (Python 3.12+). Downloads the NSIS 3.13 portable ZIP, refuses it unless
its SHA-256 matches the value fixed here, and unpacks it into `.local/tools/nsis-3.13/`
(ignored by Git), the place `delta-cli` looks for `makensis.exe`. `DELTA_MAKENSIS` can point
`delta-cli` at another copy instead; `delta.nsi` itself refuses any NSIS but 3.13.

    python -B scripts/fetch_nsis.py          # fetch when missing
    python -B scripts/fetch_nsis.py --force  # fetch again
"""

from __future__ import annotations

import argparse
import hashlib
import io
import os
import shutil
import sys
import urllib.request
import uuid
import zipfile
from pathlib import Path, PurePosixPath
from typing import Callable

REPO_ROOT = Path(__file__).resolve().parent.parent
NSIS_VERSION = "3.13"
NSIS_ZIP_URL = "https://downloads.sourceforge.net/project/nsis/NSIS%203/3.13/nsis-3.13.zip"
NSIS_ZIP_SHA256 = "ba63dffc4410ee89193e1cb5a41989991bd77c61068da17e3156d136b7b0b3d8"
NSIS_DIR = REPO_ROOT / ".local" / "tools" / f"nsis-{NSIS_VERSION}"
DOWNLOAD_TIMEOUT_SECONDS = 120.0


class FetchError(RuntimeError):
    pass


def download_bytes(url: str) -> bytes:
    request = urllib.request.Request(url, headers={"User-Agent": "delta-fetch-nsis"})
    with urllib.request.urlopen(request, timeout=DOWNLOAD_TIMEOUT_SECONDS) as response:
        return response.read()


def unpack(data: bytes, target: Path) -> None:
    """Check the archive's SHA-256, then unpack its `nsis-3.13/` tree into `target` atomically."""
    digest = hashlib.sha256(data).hexdigest()
    if digest != NSIS_ZIP_SHA256:
        raise FetchError(
            f"NSIS {NSIS_VERSION} archive SHA-256 mismatch: expected {NSIS_ZIP_SHA256}, "
            f"got {digest}; not using it"
        )
    root = f"nsis-{NSIS_VERSION}/"
    target.parent.mkdir(parents=True, exist_ok=True)
    staging = target.parent / f".{target.name}.staging-{uuid.uuid4().hex}"
    try:
        with zipfile.ZipFile(io.BytesIO(data)) as archive:
            for info in archive.infolist():
                name = info.filename.replace("\\", "/")
                if not name.startswith(root):
                    raise FetchError(f"unexpected entry in the NSIS archive: {info.filename!r}")
                relative = PurePosixPath(name[len(root):])
                if not relative.parts:
                    continue
                if relative.is_absolute() or any(
                    part == ".." or ":" in part for part in relative.parts
                ):
                    raise FetchError(f"unsafe path in the NSIS archive: {info.filename!r}")
                destination = staging.joinpath(*relative.parts)
                if info.is_dir():
                    destination.mkdir(parents=True, exist_ok=True)
                    continue
                destination.parent.mkdir(parents=True, exist_ok=True)
                with archive.open(info) as source, open(destination, "wb") as sink:
                    shutil.copyfileobj(source, sink)
        if not (staging / "makensis.exe").is_file():
            raise FetchError(f"the NSIS {NSIS_VERSION} archive has no makensis.exe")
        if target.exists():
            shutil.rmtree(target)
        os.replace(staging, target)
    finally:
        if staging.exists():
            shutil.rmtree(staging, ignore_errors=True)


def fetch(
    target: Path = NSIS_DIR,
    *,
    force: bool = False,
    download: Callable[[str], bytes] = download_bytes,
) -> Path:
    """Return `makensis.exe` under `target`, downloading the pinned build when missing."""
    makensis = target / "makensis.exe"
    if makensis.is_file() and not force:
        return makensis
    try:
        data = download(NSIS_ZIP_URL)
    except OSError as error:
        raise FetchError(
            f"could not download NSIS {NSIS_VERSION} from {NSIS_ZIP_URL}: {error}; retry when "
            "online, or unpack that archive yourself and set DELTA_MAKENSIS"
        ) from error
    unpack(data, target)
    return makensis


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--force", action="store_true", help="download again")
    args = parser.parse_args(argv)
    try:
        makensis = fetch(force=args.force)
    except FetchError as error:
        print(f"fetch_nsis: {error}", file=sys.stderr)
        return 1
    print(f"fetch_nsis: NSIS {NSIS_VERSION} ready at {makensis.relative_to(REPO_ROOT).as_posix()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
