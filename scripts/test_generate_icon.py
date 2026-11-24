"""Offline regressions for scripts/generate_icon.py."""

from __future__ import annotations

import io
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest.mock import patch

import generate_icon


class GenerateIconTests(unittest.TestCase):
    expected: dict[str, bytes] = {}

    @classmethod
    def setUpClass(cls) -> None:
        cls.expected = generate_icon.expected_assets()

    def test_committed_assets_match_generator_byte_for_byte(self) -> None:
        self.assertEqual(generate_icon.check_assets(expected=self.expected), [])
        with patch.object(generate_icon, "expected_assets", return_value=self.expected):
            out = io.StringIO()
            with redirect_stdout(out):
                code = generate_icon.main(["--check"])
        self.assertEqual(code, 0)
        self.assertIn("PASS:", out.getvalue())

    def test_check_detects_missing_or_tampered_svg_and_ico_and_write_restores(self) -> None:
        with tempfile.TemporaryDirectory(prefix="delta-icon-test-") as tmp:
            assets_dir = Path(tmp)
            with patch.object(generate_icon, "expected_assets", return_value=self.expected):
                out = io.StringIO()
                with redirect_stdout(out):
                    self.assertEqual(
                        generate_icon.main(["--assets-dir", str(assets_dir)]),
                        0,
                    )
                self.assertEqual(
                    generate_icon.main(["--check", "--assets-dir", str(assets_dir)]),
                    0,
                )

                # Flip one byte in icon.ico -> must fail --check.
                ico_path = assets_dir / "icon.ico"
                tampered_ico = bytearray(ico_path.read_bytes())
                tampered_ico[-1] ^= 0xFF
                ico_path.write_bytes(bytes(tampered_ico))
                err = io.StringIO()
                with redirect_stderr(err):
                    self.assertEqual(
                        generate_icon.main(["--check", "--assets-dir", str(assets_dir)]),
                        1,
                    )
                self.assertIn("icon.ico differs", err.getvalue())

                # Restore icon.ico, tamper icon-dev.svg -> must fail --check.
                ico_path.write_bytes(self.expected["icon.ico"])
                svg_path = assets_dir / "icon-dev.svg"
                svg_path.write_bytes(self.expected["icon-dev.svg"] + b" ")
                err = io.StringIO()
                with redirect_stderr(err):
                    self.assertEqual(
                        generate_icon.main(["--check", "--assets-dir", str(assets_dir)]),
                        1,
                    )
                self.assertIn("icon-dev.svg differs", err.getvalue())

                # Remove icon.svg -> must fail --check.
                svg_path.write_bytes(self.expected["icon-dev.svg"])
                (assets_dir / "icon.svg").unlink()
                err = io.StringIO()
                with redirect_stderr(err):
                    self.assertEqual(
                        generate_icon.main(["--check", "--assets-dir", str(assets_dir)]),
                        1,
                    )
                self.assertIn("missing icon.svg", err.getvalue())


if __name__ == "__main__":
    unittest.main()
