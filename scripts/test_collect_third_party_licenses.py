"""Offline regressions for license selection, obligation coverage and cache identity."""

import hashlib
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import collect_third_party_licenses as licenses


MIT = b"""Copyright (c) Fixture authors.
Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files, to deal in the Software.
The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software. THE SOFTWARE IS PROVIDED AS IS.
"""
ISC = b"""Copyright (c) Fixture authors.
Permission to use, copy, modify, and/or distribute this software for any purpose
with or without fee is hereby granted, provided that the above copyright notice
and this permission notice appear in all copies. THE SOFTWARE IS PROVIDED AS IS.
"""
APACHE = b"Apache License Version 2.0. Definitions.\n" + b"Standard fixture license.\n" * 20


class LicenseAuditTests(unittest.TestCase):
    def test_exact_identifiers_and_mandatory_conjunctions(self):
        expected = {
            "CC0-1.0 OR MIT-0 OR Apache-2.0": ("Apache-2.0",),
            "(Apache-2.0 OR MIT) AND BSD-3-Clause": ("MIT", "BSD-3-Clause"),
            "(MIT OR Apache-2.0) AND NCSA": ("MIT", "NCSA"),
            "Apache-2.0 AND ISC": ("Apache-2.0", "ISC"),
            "(MIT OR Apache-2.0) AND Unicode-3.0": ("MIT", "Unicode-3.0"),
            "Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT": ("MIT",),
            "MIT / Apache-2.0": ("MIT",),
            "MIT-0": ("MIT-0",),
        }
        for expression, chosen in expected.items():
            with self.subTest(expression=expression):
                self.assertEqual(licenses.selected_licenses(expression), chosen)
        for expression in ("MIT AND GPL-3.0-only", "MIT AND", "(MIT OR ISC", "MIT$", "MIT OR )"):
            with self.subTest(expression=expression), self.assertRaises(ValueError):
                licenses.selected_licenses(expression)

    def test_notice_alone_does_not_satisfy_mandatory_license(self):
        with tempfile.TemporaryDirectory(prefix="delta-license-regression-") as directory:
            base = Path(directory)
            source = base / "source"
            source.mkdir()
            (source / "Cargo.toml").write_text("[package]\nname='fixture'\n", encoding="utf-8")
            (source / "LICENSE").write_bytes(APACHE)
            (source / "NOTICE").write_text("Copyright Fixture", encoding="utf-8")
            package = {"id": "fixture-id", "name": "fixture", "version": "1.0.0",
                       "license": "Apache-2.0 AND ISC", "manifest_path": str(source / "Cargo.toml")}
            output = base / "output"
            output.mkdir()
            with patch.object(licenses, "CACHE", base / "cache"), patch.object(licenses, "fetch", return_value=ISC):
                result = licenses.collect(package, output)
            self.assertEqual(result["selected_obligations"], ["Apache-2.0", "ISC"])
            self.assertTrue(result["obligation_files"]["ISC"])
            self.assertTrue(result["standard_text_with_source_fallback"])
            originals = result["original_files"]
            self.assertTrue(any(r["file"] == "NOTICE" for r in originals))
            self.assertTrue(any(r["file"] == "original-source.zip" for r in originals))
            self.assertTrue(any(r["origin_type"] == "standard-license-text" for r in originals))

    def test_cache_is_bound_to_source_and_rejects_tampering(self):
        with tempfile.TemporaryDirectory(prefix="delta-license-regression-") as directory:
            base = Path(directory)
            source = base / "source"
            source.mkdir()
            (source / "Cargo.toml").write_text("fixture", encoding="utf-8")
            package = {"id": "fixture-id", "name": "fixture", "version": "1.0.0",
                       "license": "MIT", "manifest_path": str(source / "Cargo.toml")}
            output = base / "one"
            output.mkdir()
            with patch.object(licenses, "CACHE", base / "cache"), patch.object(licenses, "fetch", return_value=MIT):
                first = licenses.collect(package, output)
                (source / "Cargo.toml").write_text("changed source", encoding="utf-8")
                output = base / "two"
                output.mkdir()
                second = licenses.collect(package, output)
                self.assertNotEqual(first["source_id"], second["source_id"])
                cached = base / "cache" / second["source_id"] / "MIT" / "record.json"
                record = json.loads(cached.read_text(encoding="utf-8"))
                record["sha256"] = hashlib.sha256(b"tampered").hexdigest()
                cached.write_text(json.dumps(record), encoding="utf-8")
                output = base / "three"
                output.mkdir()
                with self.assertRaisesRegex(ValueError, "identity/hash"):
                    licenses.collect(package, output)


if __name__ == "__main__":
    unittest.main()
