"""Offline tests for scripts/fetch_nsis.py: no network, synthetic archives only."""

from __future__ import annotations

import hashlib
import io
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest.mock import patch

import fetch_nsis


def archive(entries: dict[str, bytes]) -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as zf:
        for name, data in entries.items():
            zf.writestr(name, data)
    return buffer.getvalue()


def pinned(data: bytes):
    """Pretend `data` is the pinned archive."""
    return patch.object(fetch_nsis, "NSIS_ZIP_SHA256", hashlib.sha256(data).hexdigest())


class FetchNsisTests(unittest.TestCase):
    def test_pinned_archive_is_unpacked_without_its_top_folder(self):
        data = archive({"nsis-3.13/makensis.exe": b"MZ", "nsis-3.13/Include/MUI2.nsh": b"x"})
        with tempfile.TemporaryDirectory() as tmp, pinned(data):
            target = Path(tmp) / "nsis-3.13"
            asked = []
            makensis = fetch_nsis.fetch(target, download=lambda url: asked.append(url) or data)
            self.assertEqual(asked, [fetch_nsis.NSIS_ZIP_URL])
            self.assertEqual(makensis.read_bytes(), b"MZ")
            self.assertTrue((target / "Include" / "MUI2.nsh").is_file())
            self.assertEqual([p.name for p in Path(tmp).iterdir()], ["nsis-3.13"])
            # Present: no second download unless forced.
            fetch_nsis.fetch(target, download=lambda url: self.fail("downloaded again"))

    def test_a_different_archive_is_refused_and_nothing_is_kept(self):
        data = archive({"nsis-3.13/makensis.exe": b"MZ"})
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / "nsis-3.13"
            with self.assertRaisesRegex(fetch_nsis.FetchError, "SHA-256 mismatch"):
                fetch_nsis.fetch(target, download=lambda url: data)
            self.assertEqual(list(Path(tmp).iterdir()), [])

    def test_unsafe_or_foreign_entries_are_refused(self):
        for entries in (
            {"nsis-3.13/../escape.txt": b"x", "nsis-3.13/makensis.exe": b"MZ"},
            {"nsis-3.13/C:evil.txt": b"x", "nsis-3.13/makensis.exe": b"MZ"},
            {"other/makensis.exe": b"MZ"},
            {"nsis-3.13/readme.txt": b"no compiler"},
        ):
            data = archive(entries)
            with self.subTest(entries=list(entries)), tempfile.TemporaryDirectory() as tmp:
                with pinned(data), self.assertRaises(fetch_nsis.FetchError):
                    fetch_nsis.fetch(Path(tmp) / "nsis-3.13", download=lambda url: data)
                self.assertEqual(list(Path(tmp).iterdir()), [])

    def test_download_failure_names_the_way_out(self):
        def offline(url: str) -> bytes:
            raise OSError("offline")

        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(fetch_nsis.FetchError, "DELTA_MAKENSIS"):
                fetch_nsis.fetch(Path(tmp) / "nsis-3.13", download=offline)

    def test_pin_matches_the_installer_template_and_cli(self):
        root = Path(__file__).resolve().parent.parent
        template = (root / "crates/delta-cli/installer/delta.nsi").read_text(encoding="utf-8")
        self.assertIn(f'!if "${{NSIS_VERSION}}" != "v{fetch_nsis.NSIS_VERSION}"', template)
        installer = (root / "crates/delta-cli/src/installer.rs").read_text(encoding="utf-8")
        self.assertIn(f'".local/tools/nsis-{fetch_nsis.NSIS_VERSION}"', installer)
        self.assertEqual(fetch_nsis.NSIS_DIR, root / ".local" / "tools" / "nsis-3.13")
        self.assertRegex(fetch_nsis.NSIS_ZIP_SHA256, r"^[0-9a-f]{64}$")


if __name__ == "__main__":
    unittest.main()
